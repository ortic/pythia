//! SQL Server sessions: the requests and events of an Oracle session, served
//! by `tiberius` on a runtime of the worker's own.
//!
//! Statements are batches. A query's rows (of its first result set) are
//! read while the worker waits for `FetchMore`s; any other request closes
//! the result, and the driver reads the rest of it before the next request.
//!
//! Break sends TDS's attention signal, which stops the batch on the server
//! at once. The server acknowledges it with a reply of its own, which
//! tiberius (0.13) doesn't read: the connection is out of step afterwards,
//! so the worker opens it again (see [`Worker::reset`]).
//!
//! Transactions behave like Oracle's: queries see what others committed,
//! and changes wait for Commit unless the session autocommits (a change
//! opens a transaction of its own). The server says whether one is open
//! (`@@TRANCOUNT`).

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use futures_util::{FutureExt, StreamExt, TryStreamExt};
use tiberius::{
    AuthMethod, ColumnData, ColumnType, EncryptionLevel, Query, QueryItem, QueryStream, SqlBrowser,
};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tracing::{debug, info, warn};

use crate::compat::Release;
use crate::dialect::Dialect;
use crate::meta::{MetaQuery, MetaResult};
use crate::session::{CancelSlot, Canceller, Events};
use crate::sql_text::{
    WHOLE_BATCH_BYTES, civil_from_days, has_word, hex, leading_words, line_offset, release,
    replace_binds, row_bytes, statement_kind,
};
use crate::ssh::Tunnel;
use crate::types::{
    Cell, Client, Column, ConnectParams, DbError, DbEvent, DbRequest, ExecSummary, ServerInfo,
};

pub(crate) type Tds = tiberius::Client<Compat<TcpStream>>;

const DEFAULT_PORT: u16 = 1433;

/// Session options of SQL Server Management Studio, which scripts expect.
const SESSION_OPTIONS: &str = "set ansi_nulls on; set ansi_padding on; set ansi_warnings on; \
                               set arithabort on; set concat_null_yields_null on; \
                               set quoted_identifier on; set ansi_null_dflt_on on; \
                               set implicit_transactions off; set xact_abort off";

pub(crate) fn run(
    params: ConnectParams,
    requests: Receiver<DbRequest>,
    events: Events,
    shared: CancelSlot,
) {
    info!(?params, "connecting");
    let connected = runtime()
        .map_err(|e| DbError::new(format!("Can't start the SQL Server client: {e}")))
        .and_then(|rt| {
            let config = config(&params)?;
            let client =
                connect(&rt, &config, params.ssh.as_deref()).map_err(|e| params.explain(e))?;
            Ok((rt, config, client))
        });
    let (rt, config, client) = match connected {
        Ok(connected) => connected,
        Err(error) => {
            warn!("connect failed: {error}");
            events.send(DbEvent::ConnectFailed(error));
            return;
        }
    };
    let cancel = Arc::new(Notify::new());
    let canceller: Canceller = {
        let cancel = cancel.clone();
        Arc::new(move || cancel.notify_one())
    };
    *shared.lock().expect("cancel mutex") = Some(canceller);

    let mut worker = Worker {
        rt,
        client,
        config,
        tunnel: params.ssh,
        init_sql: params.init_sql,
        events,
        cancel,
        release: (0, 0),
        autocommit: false,
        pending: false,
    };
    match worker.prepare_session() {
        Ok((server, failed)) => {
            worker.release = server.release;
            worker.events.send(DbEvent::Connected(server));
            for notice in failed {
                worker.events.send(DbEvent::Notice(notice));
            }
        }
        Err(e) => {
            *shared.lock().expect("cancel mutex") = None;
            worker.events.send(DbEvent::ConnectFailed(e));
            return;
        }
    }

    // A request that ended an open result set, handled next.
    let mut next = None;
    loop {
        let request = match next.take() {
            Some(request) => request,
            None => match requests.recv() {
                Ok(request) => request,
                Err(_) => break,
            },
        };
        debug!(?request, "db request");
        match request {
            DbRequest::Execute {
                id,
                sql,
                max_rows,
                binds,
            } => next = worker.execute(id, &sql, max_rows, &binds, &requests, false),
            DbRequest::ExecuteWhole { id, sql, max_rows } => {
                next = worker.execute(id, &sql, max_rows, &[], &requests, true)
            }
            // Its result was closed by another request in between.
            DbRequest::FetchMore { id, .. } => worker.events.send(DbEvent::Rows {
                id,
                rows: Vec::new(),
                more: false,
                elapsed: Duration::ZERO,
            }),
            DbRequest::Meta { id, query } => {
                let started = Instant::now();
                let result = crate::mssql_meta::run(&mut worker.ms(), &query);
                debug!(
                    id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    ok = result.is_ok(),
                    ?query,
                    "metadata query done"
                );
                worker.events.send(DbEvent::Meta { id, result });
            }
            DbRequest::Commit => worker.end_transaction(true),
            DbRequest::Rollback => worker.end_transaction(false),
            DbRequest::SetAutocommit(on) => worker.autocommit = on,
            DbRequest::Task { id, .. } => worker.events.send(DbEvent::TaskDone {
                id,
                result: Err(DbError::unsupported("This tool", Dialect::SQLSERVER)),
            }),
            DbRequest::Run { id, task } => {
                let started = Instant::now();
                let result = task(&mut MsClient(worker.ms()));
                debug!(
                    id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    ok = result.is_ok(),
                    "task done"
                );
                worker.events.send(DbEvent::TaskDone { id, result });
            }
            DbRequest::Disconnect => break,
        }
    }

    *shared.lock().expect("cancel mutex") = None;
    // Closing the connection rolls back what wasn't committed.
    let Worker {
        rt, client, events, ..
    } = worker;
    let _ =
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), client.close()).await });
    info!("disconnected");
    events.send(DbEvent::Disconnected);
}

fn runtime() -> std::io::Result<Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// Connects, through `tunnel` if there is one, following an Azure
/// gateway's redirect.
fn connect(
    rt: &Runtime,
    config: &tiberius::Config,
    tunnel: Option<&Tunnel>,
) -> Result<Tds, DbError> {
    let mut config = config.clone();
    // The first answer may name the server to connect to instead.
    for _ in 0..2 {
        // Outside the runtime: the tunnel blocks while it logs in.
        let via = tunnel.map(|t| forward(t, &config)).transpose()?;
        let connecting = attempt(config.clone(), via);
        match rt.block_on(async { tokio::time::timeout(Duration::from_secs(15), connecting).await })
        {
            Err(_) => return Err(DbError::new("Timed out connecting to SQL Server")),
            Ok(Err(tiberius::error::Error::Routing { host, port })) => {
                config.host(host);
                config.port(port);
            }
            Ok(connected) => return connected.map_err(connect_error),
        }
    }
    Err(DbError::new(
        "SQL Server redirected the connection more than once",
    ))
}

/// Connects to the server, or (through SSH) to the local end of a forward
/// to it; TLS and the login still name the server.
async fn attempt(config: tiberius::Config, via: Option<SocketAddr>) -> tiberius::Result<Tds> {
    let tcp = match via {
        Some(local) => TcpStream::connect(local).await?,
        // Finds a named instance's port through the SQL Server Browser.
        None => TcpStream::connect_named(&config).await?,
    };
    tcp.set_nodelay(true)?;
    tiberius::Client::connect(config, tcp.compat_write()).await
}

/// The local end of a forward to the server's `host:port`.
fn forward(tunnel: &Tunnel, config: &tiberius::Config) -> Result<SocketAddr, DbError> {
    let address = config.get_addr();
    let (host, port) = address
        .rsplit_once(':')
        .and_then(|(host, port)| Some((host, port.parse().ok()?)))
        .ok_or_else(|| DbError::new(format!("Not a server address: {address}")))?;
    tunnel.forward(host, port)
}

/// A failed connect, with what to do about an untrusted certificate.
fn connect_error(e: tiberius::error::Error) -> DbError {
    let tls = matches!(e, tiberius::error::Error::Tls(_))
        || e.to_string().to_lowercase().contains("certificate");
    let mut error = DbError::from(e);
    if tls {
        error.message = format!(
            "{}\n\nTo trust the server's certificate as it is (e.g. a self-signed one), add \
             ?trust to the database, like localhost:1433/master?trust.",
            error.message.lines().next().unwrap_or_default()
        );
    }
    error
}

struct Worker {
    rt: Runtime,
    client: Tds,
    config: tiberius::Config,
    /// Connecting again goes through it too.
    tunnel: Option<Arc<Tunnel>>,
    init_sql: Vec<String>,
    events: Events,
    /// Break: notified from the UI thread.
    cancel: Arc<Notify>,
    release: Release,
    autocommit: bool,
    /// The server has an open transaction (`@@TRANCOUNT > 0`).
    pending: bool,
}

/// How [`Worker::stream_statement`] reads a query's rows.
struct Fetch<'a> {
    id: u64,
    max_rows: usize,
    /// Binary values whole, for a file (#204).
    whole: bool,
    started: Instant,
    requests: &'a Receiver<DbRequest>,
}

/// How a statement ended: a query's rows were reported as they were
/// fetched, anything else is summarized.
enum Outcome {
    Query,
    Done,
}

/// How reading a batch's reply ended.
enum Ran {
    Outcome(Outcome),
    /// Break, with the statement's message: the batch is to be cancelled.
    Break(&'static str),
}

/// Why a statement didn't end well.
enum Failure {
    /// The statement failed; the session is fine.
    Statement(DbError),
    /// Break stopped the statement; `reopen` if the connection is out of
    /// step since.
    Cancelled { message: &'static str, reopen: bool },
    /// The connection is out of step with the server.
    Broken(DbError),
}

impl Worker {
    fn ms(&mut self) -> Ms<'_> {
        Ms {
            rt: &self.rt,
            client: &mut self.client,
        }
    }

    /// Sets the session up and runs the login statements, then reads what
    /// the server is: a login script may change the schema (#103). The
    /// statements' failures come back as notices.
    fn prepare_session(&mut self) -> Result<(ServerInfo, Vec<String>), DbError> {
        self.ms().execute(SESSION_OPTIONS)?;
        let mut failed = Vec::new();
        for sql in self.init_sql.clone() {
            if let Err(e) = self.ms().execute(&sql) {
                failed.push(format!(
                    "Login script: {e} ({})",
                    sql.lines().next().unwrap_or_default()
                ));
            }
        }
        let row = self
            .ms()
            .rows(
                "select cast(serverproperty('ProductVersion') as nvarchar(128)), @@version, \
                        schema_name()",
                &[],
            )?
            .into_iter()
            .next()
            .ok_or_else(|| DbError::new("The server didn't report its version"))?;
        let mut values = row.into_iter().map(Option::unwrap_or_default);
        let version = values.next().unwrap_or_default();
        let banner = values.next().unwrap_or_default();
        let server = ServerInfo {
            dialect: Dialect::SQLSERVER,
            release: release(&version),
            banner: banner.lines().next().unwrap_or_default().trim().to_owned(),
            version,
            current_schema: values.next().unwrap_or_default(),
        };
        Ok((server, failed))
    }

    /// Connects again when the connection got out of step (`why`); what
    /// wasn't committed is gone with the old one, as are temporary tables
    /// and the session's settings.
    fn reset(&mut self, why: &str) {
        warn!("opening the connection again: {why}");
        let connected = connect(&self.rt, &self.config, self.tunnel.as_deref()).map_err(|e| {
            match &self.tunnel {
                Some(tunnel) => tunnel.explain(e),
                None => e,
            }
        });
        let prepared = connected.and_then(|client| {
            self.client = client;
            self.prepare_session()
        });
        let rolled_back = if self.pending {
            "; changes that weren't committed were rolled back"
        } else {
            ""
        };
        self.pending = false;
        match prepared {
            Ok((_, failed)) => {
                for notice in failed {
                    self.events.send(DbEvent::Notice(notice));
                }
                self.events.send(DbEvent::Notice(format!(
                    "{why}: the connection was opened again{rolled_back}"
                )));
            }
            Err(e) => self
                .events
                .send(DbEvent::Error(DbError { lost: true, ..e })),
        }
    }

    /// Runs a statement. A query's result stays open for `FetchMore`s;
    /// returns the request that closed it, if any. Binary values come
    /// `whole` or cut short for the grid.
    fn execute(
        &mut self,
        id: u64,
        sql: &str,
        max_rows: usize,
        binds: &[(String, Option<String>)],
        requests: &Receiver<DbRequest>,
        whole: bool,
    ) -> Option<DbRequest> {
        let started = Instant::now();
        let mut next = None;
        // Changes wait for Commit: they get a transaction of their own.
        let opened = !self.autocommit && !self.pending && changes(sql);
        if opened {
            if let Err(error) = self.ms().execute("begin transaction") {
                self.events.send(DbEvent::Failed {
                    id,
                    error,
                    elapsed: started.elapsed(),
                });
                return None;
            }
            self.pending = true;
        }
        // A Break pressed while nothing ran doesn't stop this statement.
        let _ = self.cancel.notified().now_or_never();
        let fetch = Fetch {
            id,
            max_rows,
            whole,
            started,
            requests,
        };
        let ran = self.run_statement(sql, binds, fetch, &mut next);
        let failed = match ran {
            Ok(outcome) => {
                let counts = self.counts();
                if let Outcome::Done = outcome {
                    let kind = statement_kind(sql);
                    let summary = ExecSummary {
                        rows_affected: if is_dml(&kind) {
                            counts.as_ref().ok().map(|c| c.0)
                        } else {
                            None
                        },
                        kind,
                        compiled: None,
                    };
                    self.events.send(DbEvent::Executed {
                        id,
                        summary,
                        elapsed: started.elapsed(),
                    });
                }
                false
            }
            Err(Failure::Statement(error)) => {
                self.events.send(DbEvent::Failed {
                    id,
                    error,
                    elapsed: started.elapsed(),
                });
                true
            }
            Err(Failure::Cancelled { message, reopen }) => {
                self.events.send(DbEvent::Failed {
                    id,
                    error: DbError::new(message),
                    elapsed: started.elapsed(),
                });
                if reopen {
                    self.reset("After a cancel, SQL Server's driver can't go on");
                }
                true
            }
            Err(Failure::Broken(error)) => {
                let why = format!("The connection got out of step ({})", error.message);
                self.events.send(DbEvent::Failed {
                    id,
                    error,
                    elapsed: started.elapsed(),
                });
                self.reset(&why);
                true
            }
        };
        if failed && opened {
            // Nothing else to keep in a transaction of its own.
            let _ = self.ms().execute("if @@trancount > 0 rollback");
        }
        if let Ok((_, open)) = self.counts() {
            self.pending = open;
        }
        self.report_transaction();
        next
    }

    /// `@@ROWCOUNT` of the last statement and whether a transaction is open.
    fn counts(&mut self) -> Result<(u64, bool), DbError> {
        let row = self
            .ms()
            .rows("select cast(@@rowcount as bigint), @@trancount", &[])?;
        let mut values = row.into_iter().next().unwrap_or_default().into_iter();
        let rows = values.next().flatten().and_then(|r| r.parse().ok());
        let open = values.next().flatten().is_some_and(|t| t != "0");
        Ok((rows.unwrap_or(0), open))
    }

    fn run_statement(
        &mut self,
        sql: &str,
        binds: &[(String, Option<String>)],
        fetch: Fetch<'_>,
        next: &mut Option<DbRequest>,
    ) -> Result<Outcome, Failure> {
        // Cancelled once the batch's reply (and its hold on the client) is
        // gone.
        match self.stream_statement(sql, binds, fetch, next)? {
            Ran::Outcome(outcome) => Ok(outcome),
            Ran::Break(message) => Err(cancel_running(&self.rt, &mut self.client, message)),
        }
    }

    fn stream_statement(
        &mut self,
        sql: &str,
        binds: &[(String, Option<String>)],
        fetch: Fetch<'_>,
        next: &mut Option<DbRequest>,
    ) -> Result<Ran, Failure> {
        let Fetch {
            id,
            max_rows,
            whole,
            started,
            requests,
        } = fetch;
        let Worker {
            rt,
            client,
            events,
            cancel,
            ..
        } = self;
        let failed = |e: tiberius::error::Error| statement_failure(e, sql, binds.is_empty());
        let start: Pin<Box<dyn Future<Output = tiberius::Result<QueryStream<'_>>> + '_>> =
            if binds.is_empty() {
                Box::pin(client.simple_query(sql))
            } else {
                let mut values = Vec::new();
                let sql = replace_binds(sql, binds, |n, value| {
                    values.push(value.clone());
                    format!("@P{n}")
                });
                let mut query = Query::new(sql);
                for value in values {
                    query.bind(value);
                }
                Box::pin(query.query(client))
            };
        // Starting waits for the first result's columns: a long batch
        // runs meanwhile, and Break stops it.
        let started_stream = rt.block_on(async {
            tokio::select! {
                biased;
                stream = start => Some(stream),
                () = cancel.notified() => None,
            }
        });
        let mut stream = match started_stream {
            Some(stream) => stream.map_err(failed)?,
            None => return Ok(Ran::Break("Cancelled")),
        };

        let columns = loop {
            let item = wait(rt, cancel, &mut stream);
            report_messages(&mut stream, events);
            match item {
                Wait::Item(Some(Ok(QueryItem::Metadata(meta)))) => {
                    break meta.columns().iter().map(column).collect::<Vec<_>>();
                }
                Wait::Item(Some(Ok(QueryItem::Row(_)))) => {}
                Wait::Item(Some(Err(e))) => return Err(failed(e)),
                Wait::Item(None) => return Ok(Ran::Outcome(Outcome::Done)),
                Wait::Cancel => return Ok(Ran::Break("Cancelled")),
            }
        };
        events.send(DbEvent::QueryStarted { id, columns });

        let mut max_rows = max_rows;
        let mut started = started;
        loop {
            let mut rows = Vec::with_capacity(max_rows.min(10_000));
            let mut more = true;
            let mut cancelled = false;
            // Result sets after the first.
            let mut sets = 0;
            let mut bytes = 0;
            while rows.len() < max_rows && bytes < WHOLE_BATCH_BYTES {
                match wait(rt, cancel, &mut stream) {
                    Wait::Item(Some(Ok(QueryItem::Row(row)))) => {
                        let cells = cells(&row, whole);
                        if whole {
                            bytes += row_bytes(&cells);
                        }
                        rows.push(cells);
                    }
                    // The next result set: the first one is complete.
                    Wait::Item(Some(Ok(QueryItem::Metadata(_)))) => {
                        sets = 1;
                        more = false;
                        break;
                    }
                    Wait::Item(None) => {
                        more = false;
                        break;
                    }
                    Wait::Item(Some(Err(e))) => {
                        report_messages(&mut stream, events);
                        return Err(failed(e));
                    }
                    Wait::Cancel => {
                        cancelled = true;
                        break;
                    }
                }
            }
            if cancelled {
                events.send(DbEvent::Rows {
                    id,
                    rows,
                    more: false,
                    elapsed: started.elapsed(),
                });
                report_messages(&mut stream, events);
                return Ok(Ran::Break("Cancelled while fetching"));
            }
            events.send(DbEvent::Rows {
                id,
                rows,
                more,
                elapsed: started.elapsed(),
            });
            report_messages(&mut stream, events);
            if !more {
                // The rest of the batch runs; only its first result shows.
                while let Some(item) = rt.block_on(stream.next()) {
                    report_messages(&mut stream, events);
                    match item {
                        Ok(QueryItem::Metadata(_)) => sets += 1,
                        Ok(QueryItem::Row(_)) => {}
                        Err(e) => {
                            events.send(DbEvent::Error(failed_error(e, sql)));
                            break;
                        }
                    }
                }
                if sets > 0 {
                    events.send(DbEvent::Notice(format!(
                        "The batch returned {sets} more result set(s); only the first is shown"
                    )));
                }
                return Ok(Ran::Outcome(Outcome::Query));
            }
            match requests.recv() {
                Ok(DbRequest::FetchMore {
                    id: wanted,
                    max_rows: n,
                }) if wanted == id => {
                    max_rows = n;
                    started = Instant::now();
                }
                other => {
                    // The driver reads the rest before the next request.
                    *next = Some(other.unwrap_or(DbRequest::Disconnect));
                    return Ok(Ran::Outcome(Outcome::Query));
                }
            }
        }
    }

    fn end_transaction(&mut self, commit: bool) {
        let sql = if commit {
            "while @@trancount > 0 commit"
        } else {
            "if @@trancount > 0 rollback"
        };
        match self.ms().execute(sql) {
            Ok(()) => self.events.send(DbEvent::Notice(
                if commit { "Committed" } else { "Rolled back" }.into(),
            )),
            Err(e) => self.events.send(DbEvent::Error(e)),
        }
        if let Ok((_, open)) = self.counts() {
            self.pending = open;
        }
        self.report_transaction();
    }

    fn report_transaction(&self) {
        self.events.send(DbEvent::Transaction {
            pending: self.pending,
        });
    }
}

enum Wait {
    Item(Option<tiberius::Result<QueryItem>>),
    Cancel,
}

/// The stream's next item, or Break. The stream keeps what it has read of
/// an item when Break comes first.
fn wait(rt: &Runtime, cancel: &Notify, stream: &mut QueryStream<'_>) -> Wait {
    rt.block_on(async {
        tokio::select! {
            biased;
            item = stream.next() => Wait::Item(item),
            () = cancel.notified() => Wait::Cancel,
        }
    })
}

/// The batch's `PRINT` output and informational messages read so far, as
/// messages (#98). Reported after what came before them: the items read,
/// and rows once they are sent.
fn report_messages(stream: &mut QueryStream<'_>, events: &Events) {
    for message in stream.take_messages() {
        events.send(DbEvent::Notice(message.message().to_owned()));
    }
}

/// Break: the attention signal stops the batch on the server. The server
/// first ends its reply to the batch, then acknowledges in a reply of its
/// own, which tiberius doesn't read; then the connection needs opening
/// again.
fn cancel_running(rt: &Runtime, client: &mut Tds, message: &'static str) -> Failure {
    let reopen = match rt.block_on(client.cancel_query()) {
        Ok(()) => false,
        Err(e) => {
            debug!("after the attention: {e}");
            true
        }
    };
    Failure::Cancelled { message, reopen }
}

/// A statement's error, placed at its line in the batch.
fn failed_error(e: tiberius::error::Error, sql: &str) -> DbError {
    let line = match &e {
        tiberius::error::Error::Server(token) if token.procedure().is_empty() => {
            usize::try_from(token.line()).ok()
        }
        _ => None,
    };
    let mut error = DbError::from(e);
    error.offset = line.and_then(|l| line_offset(sql, l));
    error
}

fn statement_failure(e: tiberius::error::Error, sql: &str, as_written: bool) -> Failure {
    let broken = !matches!(e, tiberius::error::Error::Server(_));
    let mut error = failed_error(e, sql);
    if !as_written {
        error.offset = None;
    }
    if broken && !error.lost {
        Failure::Broken(error)
    } else {
        Failure::Statement(error)
    }
}

/// A session's connection for queries of its own (dictionary lookups,
/// tools): values as text, `@P1`, `@P2` bound as strings.
pub(crate) struct Ms<'a> {
    rt: &'a Runtime,
    client: &'a mut Tds,
}

impl Ms<'_> {
    /// Runs a batch, ignoring what it returns. Without parameters it runs
    /// in the session itself, so its `SET`s stay.
    pub(crate) fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        self.query(sql, &[]).map(drop)
    }

    /// The column names and the rows of the first result set of a batch.
    pub(crate) fn query(
        &mut self,
        sql: &str,
        params: &[&str],
    ) -> Result<(Vec<Column>, Vec<Vec<Cell>>), DbError> {
        let client = &mut *self.client;
        self.rt
            .block_on(async {
                let mut stream = if params.is_empty() {
                    client.simple_query(sql).await?
                } else {
                    let mut query = Query::new(sql);
                    for param in params {
                        query.bind(param.to_string());
                    }
                    query.query(client).await?
                };
                let mut columns = None;
                let mut rows = Vec::new();
                while let Some(item) = stream.try_next().await? {
                    match item {
                        QueryItem::Metadata(meta) if columns.is_none() => {
                            columns = Some(meta.columns().iter().map(column).collect());
                        }
                        QueryItem::Row(row) if row.result_index() == 0 => {
                            rows.push(cells(&row, false))
                        }
                        _ => {}
                    }
                }
                Ok::<_, tiberius::error::Error>((columns.unwrap_or_default(), rows))
            })
            .map_err(DbError::from)
    }

    pub(crate) fn rows(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Cell>>, DbError> {
        Ok(self.query(sql, params)?.1)
    }
}

/// A SQL Server connection as a [`Client`].
struct MsClient<'a>(Ms<'a>);

impl Client for MsClient<'_> {
    fn dialect(&self) -> Dialect {
        Dialect::SQLSERVER
    }

    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Cell>>, DbError> {
        self.0.rows(sql, params)
    }

    fn meta(&mut self, query: &MetaQuery) -> Result<MetaResult, DbError> {
        crate::mssql_meta::run(&mut self.0, query)
    }
}

fn is_dml(kind: &str) -> bool {
    matches!(kind, "INSERT" | "UPDATE" | "DELETE" | "MERGE")
}

/// Whether a batch changes or locks rows, so it opens a transaction when
/// the session doesn't autocommit. DDL doesn't: it would hold its locks
/// until Commit.
fn changes(sql: &str) -> bool {
    const CHANGES: &[&str] = &[
        "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "EXEC", "EXECUTE", "BULK",
    ];
    const LOCKS: &[&str] = &["UPDLOCK", "XLOCK", "HOLDLOCK", "TABLOCKX"];
    let words = leading_words(sql);
    let first = words.first().map(String::as_str).unwrap_or_default();
    let second = words.get(1).map(String::as_str).unwrap_or_default();
    match first {
        // Statistics, not rows.
        "UPDATE" if second == "STATISTICS" => false,
        _ if CHANGES.contains(&first) => true,
        "BEGIN" if matches!(second, "TRAN" | "TRANSACTION" | "DISTRIBUTED") => false,
        "SELECT" => has_word(sql, LOCKS),
        "WITH" | "DECLARE" | "BEGIN" | "IF" | "WHILE" | "SET" => {
            has_word(sql, CHANGES) || has_word(sql, LOCKS)
        }
        _ => false,
    }
}

/// Connection options from `host[\instance][:port][/database][?trust]`
/// (`?encrypt=off|strict` too), or an ADO.NET connection string
/// (`Server=…;Database=…;TrustServerCertificate=true`). Through SSH, the
/// host is as the SSH server sees it.
pub(crate) fn config(params: &ConnectParams) -> Result<tiberius::Config, DbError> {
    let text = params.connect_string.trim();
    let ado = text.contains(';') || text.contains('=') && !text.contains('?');
    // A named instance's port comes from the SQL Server Browser, over UDP,
    // which a tunnel doesn't carry.
    if params.ssh.is_some() && instance_named(text, ado)? {
        return Err(DbError::new(
            "Through SSH, give the instance's port (host:port) rather than host\\instance: \
             the SQL Server Browser that tells it can't be reached through the tunnel",
        ));
    }
    let mut config = if ado {
        tiberius::Config::from_ado_string(text)
            .map_err(|e| DbError::new(format!("Can't read the connection string: {e}")))?
    } else {
        let target = parse_target(text)?;
        let mut config = tiberius::Config::new();
        config.host(&target.host);
        config.port(target.port.unwrap_or(DEFAULT_PORT));
        if let Some(instance) = &target.instance {
            config.instance_name(instance);
        }
        if let Some(database) = &target.database {
            config.database(database);
        }
        config.encryption(target.encryption);
        if target.trust {
            config.trust_cert();
        }
        config
    };
    if !params.username.is_empty() {
        config.authentication(AuthMethod::sql_server(&params.username, &params.password));
    }
    config.application_name("Pythia");
    Ok(config)
}

/// Whether the address names an instance (`host\instance`, or the ADO.NET
/// string's server).
fn instance_named(text: &str, ado: bool) -> Result<bool, DbError> {
    if !ado {
        return Ok(parse_target(text)?.instance.is_some());
    }
    Ok(text.split(';').any(|pair| {
        pair.split_once('=').is_some_and(|(key, value)| {
            let key = key.trim().to_ascii_lowercase();
            [
                "server",
                "data source",
                "address",
                "addr",
                "network address",
            ]
            .contains(&key.as_str())
                && value.contains('\\')
        })
    }))
}

#[derive(Debug, PartialEq)]
struct Target {
    host: String,
    instance: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    encryption: EncryptionLevel,
    trust: bool,
}

fn parse_target(text: &str) -> Result<Target, DbError> {
    let text = ["sqlserver://", "mssql://"]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .unwrap_or(text);
    let (address, query) = text.split_once('?').unwrap_or((text, ""));
    let (host_port, database) = match address.split_once('/') {
        Some((host_port, db)) => (host_port, Some(db.trim()).filter(|d| !d.is_empty())),
        None => (address, None),
    };
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| DbError::new(format!("Missing ']' in {host_port}")))?;
        (host, rest.strip_prefix(':'))
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    let port = port
        .map(|p| {
            p.parse()
                .map_err(|_| DbError::new(format!("Not a port number: {p}")))
        })
        .transpose()?;
    let (host, instance) = match host.split_once('\\') {
        Some((host, instance)) => (host, Some(instance.to_owned())),
        None => (host, None),
    };
    let host = if host.is_empty() || host == "." {
        "localhost"
    } else {
        host
    };
    let mut target = Target {
        host: host.to_owned(),
        instance,
        port,
        database: database.map(str::to_owned),
        encryption: EncryptionLevel::Required,
        trust: false,
    };
    for option in query.split('&').filter(|o| !o.is_empty()) {
        match option.to_ascii_lowercase().as_str() {
            "trust" | "trustservercertificate" | "trustservercertificate=true" => {
                target.trust = true
            }
            "encrypt=on" | "encrypt=yes" | "encrypt=true" | "encrypt=mandatory" => {
                target.encryption = EncryptionLevel::Required
            }
            "encrypt=off" | "encrypt=no" | "encrypt=false" | "encrypt=optional" => {
                target.encryption = EncryptionLevel::Off
            }
            "encrypt=strict" => target.encryption = EncryptionLevel::Strict,
            _ => {
                return Err(DbError::new(format!(
                    "Unknown option {option} (use trust, encrypt=off, encrypt=strict)"
                )));
            }
        }
    }
    Ok(target)
}

pub(crate) fn column(c: &tiberius::Column) -> Column {
    use ColumnType::*;
    let ty = c.column_type();
    let numeric = matches!(
        ty,
        Int1 | Int2
            | Int4
            | Int8
            | Intn
            | Float4
            | Float8
            | Floatn
            | Money
            | Money4
            | Decimaln
            | Numericn
    );
    let name = match ty {
        Null => "null",
        Bit | Bitn => "bit",
        Int1 => "tinyint",
        Int2 => "smallint",
        Int4 | Intn => "int",
        Int8 => "bigint",
        Datetime4 => "smalldatetime",
        Datetime | Datetimen => "datetime",
        Float4 => "real",
        Float8 | Floatn => "float",
        Money => "money",
        Money4 => "smallmoney",
        Guid => "uniqueidentifier",
        Decimaln => "decimal",
        Numericn => "numeric",
        Daten => "date",
        Timen => "time",
        Datetime2 => "datetime2",
        DatetimeOffsetn => "datetimeoffset",
        BigVarBin => "varbinary",
        BigVarChar => "varchar",
        BigBinary => "binary",
        BigChar => "char",
        NVarchar => "nvarchar",
        NChar => "nchar",
        Xml => "xml",
        Udt => "udt",
        Text => "text",
        Image => "image",
        NText => "ntext",
        SSVariant => "sql_variant",
    };
    Column {
        name: c.name().to_owned(),
        type_name: name.to_owned(),
        numeric,
    }
}

fn cells(row: &tiberius::Row, whole: bool) -> Vec<Cell> {
    row.cells().map(|(_, data)| cell(data, whole)).collect()
}

/// A value as SQL Server Management Studio shows it; binary values cut
/// short unless `whole`.
fn cell(data: &ColumnData<'_>, whole: bool) -> Cell {
    // Days from 0001-01-01 and 1900-01-01 to 1970-01-01.
    const FROM_YEAR_1: i64 = -719_162;
    const FROM_1900: i64 = -25_567;
    Some(match data {
        ColumnData::U8(v) => v.as_ref()?.to_string(),
        ColumnData::I16(v) => v.as_ref()?.to_string(),
        ColumnData::I32(v) => v.as_ref()?.to_string(),
        ColumnData::I64(v) => v.as_ref()?.to_string(),
        ColumnData::F32(v) => v.as_ref()?.to_string(),
        ColumnData::F64(v) => v.as_ref()?.to_string(),
        ColumnData::Bit(v) => if (*v)? { "1" } else { "0" }.to_owned(),
        ColumnData::String(v) => v.as_deref()?.to_owned(),
        ColumnData::Guid(v) => v.as_ref()?.to_string().to_uppercase(),
        ColumnData::Binary(v) => format!("0x{}", hex(v.as_deref()?, whole)),
        ColumnData::Numeric(v) => v.as_ref()?.to_string(),
        ColumnData::Xml(v) => v.as_deref()?.to_string(),
        ColumnData::DateTime(v) => {
            let v = v.as_ref()?;
            // 1/300 seconds since midnight.
            let ms = (u64::from(v.seconds_fragments()) * 1000 + 150) / 300;
            format!(
                "{} {}",
                date_text(i64::from(v.days()) + FROM_1900),
                time_text(ms, 3)
            )
        }
        ColumnData::SmallDateTime(v) => {
            let v = v.as_ref()?;
            let minutes = u64::from(v.seconds_fragments());
            format!(
                "{} {}",
                date_text(i64::from(v.days()) + FROM_1900),
                time_text(minutes * 60, 0)
            )
        }
        ColumnData::Date(v) => date_text(i64::from(v.as_ref()?.days()) + FROM_YEAR_1),
        ColumnData::Time(v) => {
            let v = v.as_ref()?;
            time_text(v.increments(), v.scale())
        }
        ColumnData::DateTime2(v) => {
            let v = v.as_ref()?;
            format!(
                "{} {}",
                date_text(i64::from(v.date().days()) + FROM_YEAR_1),
                time_text(v.time().increments(), v.time().scale())
            )
        }
        ColumnData::DateTimeOffset(v) => {
            let v = v.as_ref()?;
            // The value is in UTC; shown in its own offset.
            let (date, time) = (v.datetime2().date(), v.datetime2().time());
            let unit = 10i128.pow(u32::from(time.scale()));
            let day = 86_400 * unit;
            let local = i128::from(date.days()) * day
                + i128::from(time.increments())
                + i128::from(v.offset()) * 60 * unit;
            let days = i64::try_from(local.div_euclid(day)).ok()? + FROM_YEAR_1;
            let increments = u64::try_from(local.rem_euclid(day)).ok()?;
            let offset = v.offset();
            format!(
                "{} {} {}{:02}:{:02}",
                date_text(days),
                time_text(increments, time.scale()),
                if offset < 0 { '-' } else { '+' },
                offset.abs() / 60,
                offset.abs() % 60
            )
        }
    })
}

fn date_text(days_since_1970: i64) -> String {
    let (year, month, day) = civil_from_days(days_since_1970);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `increments` of 10^-`scale` seconds since midnight as `hh:mi:ss.fff…`.
fn time_text(increments: u64, scale: u8) -> String {
    let unit = 10u64.pow(u32::from(scale));
    let seconds = increments / unit;
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if scale > 0 {
        text.push_str(&format!(
            ".{:0width$}",
            increments % unit,
            width = usize::from(scale)
        ));
    }
    text
}

impl From<tiberius::error::Error> for DbError {
    fn from(e: tiberius::error::Error) -> Self {
        match e {
            tiberius::error::Error::Server(token) => {
                let mut place = format!("Line {}", token.line());
                if !token.procedure().is_empty() {
                    place = format!("Procedure {}, {place}", token.procedure());
                }
                Self {
                    code: i32::try_from(token.code()).ok(),
                    // Severity 20 and up ends the connection.
                    lost: token.class() >= 20,
                    ..Self::new(format!(
                        "Msg {}, Level {}, State {}, {place}\n{}",
                        token.code(),
                        token.class(),
                        token.state(),
                        token.message()
                    ))
                }
            }
            tiberius::error::Error::Io { message, .. } => Self {
                lost: true,
                ..Self::new(format!("Lost connection to SQL Server: {message}"))
            },
            e => Self::new(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let t = parse_target("localhost").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.database, t.trust),
            ("localhost", None, None, false)
        );
        let t = parse_target("db.example.com:1434/shop?trust").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.database.as_deref(), t.trust),
            ("db.example.com", Some(1434), Some("shop"), true)
        );
        let t = parse_target(r".\SQLEXPRESS/app?encrypt=off").unwrap();
        assert_eq!(
            (t.host.as_str(), t.instance.as_deref(), t.encryption),
            ("localhost", Some("SQLEXPRESS"), EncryptionLevel::Off)
        );
        let t = parse_target("sqlserver://[::1]:1500").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", Some(1500)));
        assert!(parse_target("h:port").is_err());
        assert!(parse_target("h?compress").is_err());
    }

    #[test]
    fn transaction_changes() {
        for (sql, changes_rows) in [
            ("select * from t", false),
            ("select * from t with (updlock)", true),
            ("update t set a = 1", true),
            ("update statistics dbo.t with fullscan", false),
            ("exec dbo.p", true),
            ("begin tran", false),
            ("declare @n int = 1; update t set a = @n", true),
            ("declare @n int = 1; select @n", false),
            (
                "create trigger tr on t after insert, update as select 1",
                false,
            ),
            ("if exists (select 1 from t) delete from t", true),
        ] {
            assert_eq!(changes(sql), changes_rows, "{sql}");
        }
    }

    #[test]
    fn times() {
        assert_eq!(time_text(0, 0), "00:00:00");
        assert_eq!(time_text(452_961_234_567, 7), "12:34:56.1234567");
        assert_eq!(time_text(45_296_123, 3), "12:34:56.123");
        assert_eq!(date_text(0), "1970-01-01");
    }
}
