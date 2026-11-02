//! PostgreSQL sessions: the requests and events of an Oracle session, served
//! by `tokio-postgres` on a runtime of the worker's own.
//!
//! Statements run with the simple query protocol, so values arrive as the
//! server writes them; their columns' types come from describing the
//! statement first. A query's rows are read while the worker waits for
//! `FetchMore`s; any other request closes the result, cancelling the query
//! when rows are left.
//!
//! Transactions behave like Oracle's: queries see what others committed and
//! hold no locks, and changes wait for Commit unless the session
//! autocommits (a change opens a transaction of its own). Inside a
//! transaction every statement gets a savepoint, so a failing statement is
//! undone alone instead of aborting the whole transaction.

use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::runtime::Runtime;
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::types::{Kind, Type};
use tokio_postgres::{
    AsyncMessage, CancelToken, Config, SimpleQueryMessage, SimpleQueryRow, SimpleQueryStream,
    Socket,
};
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::{debug, info, warn};

use crate::compat::Release;
use crate::dialect::Dialect;
use crate::meta::{MetaQuery, MetaResult};
use crate::session::{CancelSlot, Canceller, Events, Requests};
use crate::sql_text::{
    WHOLE_BATCH_BYTES, char_offset, has_inner_semicolon, has_word, leading_words, release,
    replace_binds, row_bytes, statement_kind, word_followed_by,
};
use crate::ssh::Tunnel;
use crate::types::{
    Bind, Cell, Client, Column, ConnectParams, DbError, DbEvent, DbRequest, ExecSummary,
    NameContext, ServerInfo,
};

type Connection =
    tokio_postgres::Connection<Socket, <MakeRustlsConnect as MakeTlsConnect<Socket>>::Stream>;
type Stream = Pin<Box<SimpleQueryStream>>;

/// How long the rest of an abandoned result is read before the query is
/// cancelled: a small result has arrived already.
const DRAIN: Duration = Duration::from_millis(200);
/// How long Break waits for the server to take its cancel request.
const CANCEL_WAIT: Duration = Duration::from_secs(15);
/// The savepoint each statement in a transaction runs after.
const SAVEPOINT: &str = "pythia_statement";

pub(crate) fn run(params: ConnectParams, requests: Requests, events: Events, shared: CancelSlot) {
    info!(?params, "connecting");
    let connected = runtime()
        .map_err(|e| DbError::new(format!("Can't start the PostgreSQL client: {e}")))
        .and_then(|rt| {
            let (config, tls) = config(&params)?;
            let (client, connection) = rt
                .block_on(config.connect(tls.clone()))
                .map_err(|e| params.explain(e.into()))?;
            Ok((rt, client, connection, tls))
        });
    let (rt, client, connection, tls) = match connected {
        Ok(connected) => connected,
        Err(error) => {
            warn!("connect failed: {error}");
            events.send(DbEvent::ConnectFailed(error));
            return;
        }
    };
    let (notice_tx, notices) = mpsc::channel();
    let driver = rt.spawn(drive(connection, notice_tx));
    let token = client.cancel_token();
    let cancel: Canceller = {
        let (token, tls) = (token.clone(), tls.clone());
        Arc::new(move || cancel_query(&token, tls.clone()))
    };
    *shared.lock().expect("cancel mutex") = Some(cancel);

    let mut worker = Worker {
        rt,
        client,
        token,
        tls,
        events,
        notices,
        release: (0, 0),
        autocommit: false,
        tx: Transaction::default(),
        context: NameContext::default(),
    };
    match worker.on_connect(&params.init_sql) {
        Ok((server, failed)) => {
            worker.release = server.release;
            worker.context = server.context.clone();
            worker.events.send(DbEvent::Connected(server));
            for notice in failed {
                worker.events.send(DbEvent::Notice(notice));
            }
            worker.report_notices();
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
            DbRequest::FetchMore { id, .. } => worker.events.send(DbEvent::QueryClosed { id }),
            DbRequest::Meta { id, query } => {
                let started = Instant::now();
                let result = worker.meta(&query);
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
                result: Err(DbError::unsupported("This tool", Dialect::POSTGRES)),
            }),
            DbRequest::Run { id, task } => {
                let started = Instant::now();
                let pending = worker.tx.pending;
                let result = task(&mut PgClient(worker.pg(), pending));
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
        worker.report_notices();
    }

    *shared.lock().expect("cancel mutex") = None;
    // Without a client the connection says goodbye (rolling back what wasn't
    // committed) and ends.
    let Worker {
        rt, client, events, ..
    } = worker;
    drop(client);
    let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), driver).await });
    info!("disconnected");
    events.send(DbEvent::Disconnected);
}

fn runtime() -> std::io::Result<Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// Runs the connection: its I/O happens while the worker waits for an
/// answer. Notices (`RAISE NOTICE`, "… does not exist, skipping") go to the
/// worker, which reports them after each request.
async fn drive(connection: Connection, notices: mpsc::Sender<String>) {
    let mut connection = connection;
    let mut messages = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));
    while let Some(message) = messages.next().await {
        match message {
            Ok(AsyncMessage::Notice(notice)) => {
                let _ = notices.send(notice_text(&notice));
            }
            Ok(_) => {}
            Err(e) => {
                warn!("connection ended: {e}");
                return;
            }
        }
    }
}

fn notice_text(notice: &tokio_postgres::error::DbError) -> String {
    let mut text = format!("{}: {}", notice.severity(), notice.message());
    if let Some(detail) = notice.detail() {
        text.push_str(&format!("\nDETAIL: {detail}"));
    }
    if let Some(hint) = notice.hint() {
        text.push_str(&format!("\nHINT: {hint}"));
    }
    text
}

/// PostgreSQL's way to break into a session: a request on a connection of
/// its own, on the thread [`crate::Session::cancel`] runs it on. A server
/// that doesn't answer holds it up for [`CANCEL_WAIT`] at most, so the next
/// Break gets through (#207).
fn cancel_query(token: &CancelToken, tls: MakeRustlsConnect) {
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(e) => return warn!("cancel failed: {e}"),
    };
    match rt.block_on(async { tokio::time::timeout(CANCEL_WAIT, token.cancel_query(tls)).await }) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("cancel failed: {e}"),
        Err(_) => warn!("cancel failed: no answer within {CANCEL_WAIT:?}"),
    }
}

struct Worker {
    rt: Runtime,
    client: tokio_postgres::Client,
    token: CancelToken,
    tls: MakeRustlsConnect,
    events: Events,
    notices: Receiver<String>,
    release: Release,
    autocommit: bool,
    tx: Transaction,
    /// The search path last reported (#320).
    context: NameContext,
}

/// How a statement ended: a query's rows were reported as they were
/// fetched (`failed` if fetching them did), anything else is summarized.
enum Outcome {
    Query { failed: bool },
    Done { affected: Option<u64> },
}

impl Worker {
    fn pg(&self) -> Pg<'_> {
        Pg {
            rt: &self.rt,
            client: &self.client,
            release: self.release,
        }
    }

    /// Answers a dictionary lookup or a plan (a SQL window's, #269). In a
    /// transaction it runs after a savepoint, so one that fails doesn't
    /// abort the transaction.
    fn meta(&self, query: &MetaQuery) -> Result<MetaResult, DbError> {
        if !self.tx.pending {
            return crate::pg_meta::run(&self.pg(), query);
        }
        self.pg().execute(&format!("savepoint {SAVEPOINT}"))?;
        let result = crate::pg_meta::run(&self.pg(), query);
        let back = if result.is_ok() {
            format!("release savepoint {SAVEPOINT}")
        } else {
            format!("rollback to savepoint {SAVEPOINT}; release savepoint {SAVEPOINT}")
        };
        self.pg().execute(&back)?;
        result
    }

    /// Runs the login statements, which may set the search path (#103),
    /// then reads what the server is and its current schema. The
    /// statements' failures come back as notices.
    fn on_connect(&mut self, init_sql: &[String]) -> Result<(ServerInfo, Vec<String>), DbError> {
        let mut failed = Vec::new();
        for sql in init_sql {
            if let Err(e) = self.pg().execute(sql) {
                failed.push(format!(
                    "Login script: {e} ({})",
                    sql.lines().next().unwrap_or_default()
                ));
            }
        }
        let row = self
            .pg()
            .first(
                "select current_setting('server_version'), version(), current_schema()",
                &[],
            )?
            .ok_or_else(|| DbError::new("The server didn't report its version"))?;
        let mut values = row.into_iter().map(Option::unwrap_or_default);
        let full = values.next().unwrap_or_default();
        // `18.1 (Debian 18.1-1.pgdg13+2)` → `18.1`
        let version = full
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        let server = ServerInfo {
            dialect: Dialect::POSTGRES,
            release: release(&version),
            banner: values.next().unwrap_or(full),
            version,
            current_schema: values.next().unwrap_or_default(),
            context: self.context()?,
        };
        Ok((server, failed))
    }

    /// The schemas of the search path that exist, in its order (#320).
    fn context(&self) -> Result<NameContext, DbError> {
        let rows = self.pg().rows(
            "select s from unnest(current_schemas(false)) with ordinality as p (s, n) order by n",
            &[],
        )?;
        Ok(NameContext {
            schemas: rows
                .into_iter()
                .filter_map(|r| r.into_iter().next().flatten())
                .collect(),
            ..NameContext::default()
        })
    }

    /// Reports the search path when it changed (#320).
    fn report_context(&mut self) {
        match self.context() {
            Ok(context) if context != self.context => {
                self.events.send(DbEvent::Context(context.clone()));
                self.context = context;
            }
            Ok(_) => {}
            Err(e) => warn!("reading the search path: {e}"),
        }
    }

    /// Runs a statement. A query's result stays open for `FetchMore`s;
    /// returns the request that closed it, if any. Values are whole; for
    /// a file (`whole`) a batch is also kept to [`WHOLE_BATCH_BYTES`].
    fn execute(
        &mut self,
        id: u64,
        sql: &str,
        max_rows: usize,
        binds: &[(String, Bind)],
        requests: &Requests,
        whole: bool,
    ) -> Option<DbRequest> {
        let started = Instant::now();
        let mut next = None;
        // Text is written into the statement: the simple protocol has no
        // parameters, and PostgreSQL types an unadorned literal by its use.
        // A value bound whole (#293) is a parameter, `$1`, `$2`, ….
        let mut params = Vec::new();
        let sql = if binds.is_empty() {
            sql.to_owned()
        } else {
            replace_binds(sql, binds, |_, value| match value {
                Bind::Text(text) => text.as_deref().map_or_else(|| "null".into(), literal),
                whole => {
                    params.push(whole.clone());
                    format!("${}", params.len())
                }
            })
        };
        let effect = TxEffect::of(&sql);
        // Changes wait for Commit: they get a transaction of their own.
        let opened = !self.autocommit && !self.tx.pending && effect == TxEffect::Changes;
        let guarded = self.tx.pending && effect.guarded();
        let before = if opened {
            self.pg().execute("begin")
        } else if guarded {
            self.pg().execute(&format!("savepoint {SAVEPOINT}"))
        } else {
            Ok(())
        };
        if let Err(error) = before {
            self.events.send(DbEvent::Failed {
                id,
                error,
                elapsed: started.elapsed(),
            });
            self.report_transaction();
            return None;
        }
        if opened {
            self.tx.pending = true;
        }
        let fetch = Fetch {
            id,
            max_rows,
            whole,
            started,
            requests,
        };
        let ran = if params.is_empty() {
            self.run_statement(&sql, fetch, &mut next)
        } else {
            self.run_with_params(&sql, &params)
        };
        let failed = match ran {
            Ok(Outcome::Done { affected }) => {
                let kind = statement_kind(&sql);
                let summary = ExecSummary {
                    rows_affected: if is_dml(&kind) { affected } else { None },
                    kind,
                    compiled: None,
                };
                self.events.send(DbEvent::Executed {
                    id,
                    summary,
                    elapsed: started.elapsed(),
                });
                false
            }
            Ok(Outcome::Query { failed }) => failed,
            Err(e) => {
                let position = error_position(&e);
                let mut error = DbError::from(e);
                // A position in the statement as sent, which binds changed.
                if binds.is_empty() {
                    error.offset = position.and_then(|p| char_offset(&sql, p));
                }
                self.events.send(DbEvent::Failed {
                    id,
                    error,
                    elapsed: started.elapsed(),
                });
                true
            }
        };
        if failed {
            if opened {
                // Nothing else to keep in a transaction of its own.
                let _ = self.pg().execute("rollback");
                self.tx.pending = false;
            } else if guarded {
                let _ = self.pg().execute(&format!(
                    "rollback to savepoint {SAVEPOINT}; release savepoint {SAVEPOINT}"
                ));
            } else if effect == TxEffect::Ends {
                // A failed COMMIT rolls back.
                self.tx.pending = false;
            }
        } else {
            if guarded {
                let _ = self.pg().execute(&format!("release savepoint {SAVEPOINT}"));
            }
            self.tx.after(effect);
        }
        if changes_path(&sql) {
            self.report_context();
        }
        self.report_notices();
        self.report_transaction();
        next
    }

    /// Runs a statement with parameters, which only the extended protocol
    /// has, as a statement without rows: bytes as bytea, text as text.
    fn run_with_params(
        &mut self,
        sql: &str,
        params: &[Bind],
    ) -> Result<Outcome, tokio_postgres::Error> {
        let values: Vec<Box<dyn tokio_postgres::types::ToSql + Sync>> = params
            .iter()
            .map(|p| -> Box<dyn tokio_postgres::types::ToSql + Sync> {
                match p {
                    Bind::Text(text) => Box::new(text.clone()),
                    Bind::Bytes(bytes) => Box::new(bytes.to_vec()),
                    Bind::LongText(text) => Box::new(text.to_string()),
                }
            })
            .collect();
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            values.iter().map(|v| &**v).collect();
        let affected = self.rt.block_on(self.client.execute(sql, &refs))?;
        Ok(Outcome::Done {
            affected: Some(affected),
        })
    }

    fn run_statement(
        &mut self,
        sql: &str,
        fetch: Fetch<'_>,
        next: &mut Option<DbRequest>,
    ) -> Result<Outcome, tokio_postgres::Error> {
        // The simple protocol names a result's columns, not their types.
        let types = if returns_rows(sql) && !has_inner_semicolon(sql) {
            let statement = self.rt.block_on(self.client.prepare(sql))?;
            statement
                .columns()
                .iter()
                .map(|c| c.type_().clone())
                .collect()
        } else {
            Vec::new()
        };
        let mut stream: Stream = Box::pin(self.rt.block_on(self.client.simple_query_raw(sql))?);
        let mut affected = None;
        while let Some(message) = self.rt.block_on(stream.next()) {
            match message? {
                SimpleQueryMessage::RowDescription(described) => {
                    let columns = described
                        .iter()
                        .enumerate()
                        .map(|(i, c)| column(c.name(), types.get(i)))
                        .collect();
                    self.events.send(DbEvent::QueryStarted {
                        id: fetch.id,
                        columns,
                    });
                    let failed = self.fetch(stream, fetch, next);
                    return Ok(Outcome::Query { failed });
                }
                SimpleQueryMessage::CommandComplete(n) => affected = Some(n),
                _ => {}
            }
        }
        Ok(Outcome::Done { affected })
    }

    /// Reports a statement's results, their rows while `FetchMore`s for
    /// them come in. Returns whether the statement failed (or was cancelled
    /// when abandoned).
    fn fetch(
        &mut self,
        mut stream: Stream,
        fetch: Fetch<'_>,
        next: &mut Option<DbRequest>,
    ) -> bool {
        let Fetch {
            id,
            max_rows: batch,
            whole,
            mut started,
            requests,
        } = fetch;
        let mut max_rows = batch;
        loop {
            let mut rows = Vec::with_capacity(max_rows.min(10_000));
            let mut more = true;
            // What follows the result: the next one's columns, or the error
            // that ended the statement.
            let mut after = None;
            let mut bytes = 0;
            while rows.len() < max_rows && bytes < WHOLE_BATCH_BYTES {
                match self.rt.block_on(stream.next()) {
                    Some(Ok(SimpleQueryMessage::Row(row))) => {
                        let cells = cells(&row);
                        if whole {
                            bytes += row_bytes(&cells);
                        }
                        rows.push(cells);
                    }
                    // A rule's next query, without a CommandComplete between.
                    Some(Ok(SimpleQueryMessage::RowDescription(described))) => {
                        after = Some(Ok(untyped(&described)));
                        more = false;
                        break;
                    }
                    Some(Ok(SimpleQueryMessage::CommandComplete(_))) => {
                        more = false;
                        if !whole {
                            after = self.next_result(&mut stream);
                        }
                        break;
                    }
                    None => {
                        more = false;
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        self.events.send(DbEvent::Failed {
                            id,
                            error: e.into(),
                            elapsed: started.elapsed(),
                        });
                        return true;
                    }
                }
            }
            // A file has the first result only, and says how many others.
            if whole && !more {
                self.events.send(DbEvent::Rows {
                    id,
                    rows,
                    more,
                    elapsed: started.elapsed(),
                });
                return self.drop_rest(&mut stream, usize::from(after.is_some()), id, started);
            }
            // Several statements in one, or a rule's queries: each result is
            // one, announced before the last rows of the one before, as on
            // MySQL (#290, #316).
            if after.is_some() {
                self.events.send(DbEvent::MoreResults { id });
            }
            self.events.send(DbEvent::Rows {
                id,
                rows,
                more,
                elapsed: started.elapsed(),
            });
            match after {
                Some(Ok(columns)) => {
                    self.events.send(DbEvent::QueryStarted { id, columns });
                    max_rows = batch;
                    started = Instant::now();
                    continue;
                }
                Some(Err(e)) => {
                    self.events.send(DbEvent::Failed {
                        id,
                        error: e.into(),
                        elapsed: started.elapsed(),
                    });
                    return true;
                }
                None if !more => return false,
                None => {}
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
                    self.events.send(DbEvent::QueryClosed { id });
                    *next = Some(other.unwrap_or(DbRequest::Disconnect));
                    return self.abandon(stream);
                }
            }
        }
    }

    /// Reads on to the next result of the statement, past those of
    /// statements without rows: its columns, or the error that ended the
    /// statement; `None` at its end.
    fn next_result(
        &self,
        stream: &mut Stream,
    ) -> Option<Result<Vec<Column>, tokio_postgres::Error>> {
        loop {
            match self.rt.block_on(stream.next())? {
                Ok(SimpleQueryMessage::RowDescription(described)) => {
                    return Some(Ok(untyped(&described)));
                }
                Ok(_) => {}
                Err(e) => return Some(Err(e)),
            }
        }
    }

    /// Reads the statement's results after the one written to a file (and
    /// the `seen` of them read already), and says how many there were.
    /// Returns whether one failed.
    fn drop_rest(&self, stream: &mut Stream, seen: usize, id: u64, started: Instant) -> bool {
        let mut results = seen;
        while let Some(message) = self.rt.block_on(stream.next()) {
            match message {
                Ok(SimpleQueryMessage::RowDescription(_)) => results += 1,
                Ok(_) => {}
                Err(e) => {
                    self.events.send(DbEvent::Failed {
                        id,
                        error: e.into(),
                        elapsed: started.elapsed(),
                    });
                    return true;
                }
            }
        }
        if results > 0 {
            self.events.send(DbEvent::Notice(format!(
                "The statement returned {results} more result(s); only the first was written \
                 to the file"
            )));
        }
        false
    }

    /// Closes a result with rows left: what has arrived is read, and a query
    /// still sending is cancelled. Returns whether it ended in an error.
    fn abandon(&mut self, mut stream: Stream) -> bool {
        let deadline = Instant::now() + DRAIN;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self
                .rt
                .block_on(async { tokio::time::timeout(left, stream.next()).await })
            {
                Ok(None) => return false,
                Ok(Some(Err(_))) => return true,
                Ok(Some(Ok(_))) => {}
                Err(_) => break,
            }
        }
        // Waits for the server to take the request, so it can't hit the
        // next statement.
        if let Err(e) = self.rt.block_on(self.token.cancel_query(self.tls.clone())) {
            warn!("cancel failed: {e}");
        }
        let mut failed = false;
        while let Some(message) = self.rt.block_on(stream.next()) {
            failed |= message.is_err();
        }
        failed
    }

    fn end_transaction(&mut self, commit: bool) {
        match self
            .pg()
            .execute(if commit { "commit" } else { "rollback" })
        {
            Ok(()) => {
                self.tx.pending = false;
                self.events.send(DbEvent::Notice(
                    if commit { "Committed" } else { "Rolled back" }.into(),
                ));
            }
            Err(e) => {
                // A failed commit rolls back too.
                self.tx.pending = false;
                self.events.send(DbEvent::Error(e));
            }
        }
        // A SET LOCAL ends with the transaction.
        self.report_context();
        self.report_notices();
        self.report_transaction();
    }

    fn report_transaction(&self) {
        self.events.send(DbEvent::Transaction {
            pending: self.tx.pending,
        });
    }

    /// Notices of the last request, as messages.
    fn report_notices(&self) {
        while let Ok(notice) = self.notices.try_recv() {
            self.events.send(DbEvent::Notice(notice));
        }
    }
}

/// Whether `sql` may change the schemas of the search path (#320): SET
/// and RESET, a transaction's end (SET LOCAL), DISCARD, a DO block or CALL
/// that sets it, a schema made, dropped or renamed; set_config anywhere in
/// a statement, as SET does, of the path or of the role "$user" names
/// (#362). A function's own SET search_path is undone when it returns.
fn changes_path(sql: &str) -> bool {
    let words = leading_words(sql);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(
        words[..],
        [
            "SET" | "RESET" | "DISCARD" | "COMMIT" | "END" | "ROLLBACK" | "ABORT" | "DO" | "CALL",
            ..
        ] | ["CREATE" | "DROP" | "ALTER", "SCHEMA"]
            | ["PREPARE", "TRANSACTION"]
    ) || has_word(sql, &["SET_CONFIG"])
}

/// What [`Worker::fetch`] reads.
struct Fetch<'a> {
    id: u64,
    max_rows: usize,
    /// For a file: batches kept to [`WHOLE_BATCH_BYTES`] (#204).
    whole: bool,
    started: Instant,
    requests: &'a Requests,
}

/// A session's connection for queries of its own (dictionary lookups,
/// tools): values as text, `$1`, `$2` written in as literals.
pub(crate) struct Pg<'a> {
    rt: &'a Runtime,
    client: &'a tokio_postgres::Client,
    pub(crate) release: Release,
}

impl Pg<'_> {
    /// Runs statements, ignoring what they return.
    pub(crate) fn execute(&self, sql: &str) -> Result<(), DbError> {
        Ok(self.rt.block_on(self.client.batch_execute(sql))?)
    }

    /// The column names and rows of a query.
    pub(crate) fn query(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<(Vec<String>, Vec<Vec<Cell>>), DbError> {
        let sql = with_params(sql, params);
        let messages = self.rt.block_on(self.client.simple_query(&sql))?;
        let mut names = Vec::new();
        let mut rows = Vec::new();
        for message in messages {
            match message {
                SimpleQueryMessage::RowDescription(columns) if names.is_empty() => {
                    names = columns.iter().map(|c| c.name().to_owned()).collect();
                }
                SimpleQueryMessage::Row(row) => rows.push(cells(&row)),
                _ => {}
            }
        }
        Ok((names, rows))
    }

    pub(crate) fn rows(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Cell>>, DbError> {
        Ok(self.query(sql, params)?.1)
    }

    pub(crate) fn first(&self, sql: &str, params: &[&str]) -> Result<Option<Vec<Cell>>, DbError> {
        Ok(self.rows(sql, params)?.into_iter().next())
    }
}

/// A PostgreSQL connection as a [`Client`].
/// The session's connection, and whether it has a transaction open.
struct PgClient<'a>(Pg<'a>, bool);

impl Client for PgClient<'_> {
    fn dialect(&self) -> Dialect {
        Dialect::POSTGRES
    }

    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Cell>>, DbError> {
        self.0.rows(sql, params)
    }

    fn meta(&mut self, query: &MetaQuery) -> Result<MetaResult, DbError> {
        crate::pg_meta::run(&self.0, query)
    }

    fn aborts_transaction(&self) -> bool {
        self.1
    }
}

/// The columns of a result after the first: the simple protocol names
/// them, not their types.
fn untyped(described: &[tokio_postgres::SimpleColumn]) -> Vec<Column> {
    described.iter().map(|c| column(c.name(), None)).collect()
}

fn cells(row: &SimpleQueryRow) -> Vec<Cell> {
    (0..row.len())
        .map(|i| row.get(i).map(str::to_owned))
        .collect()
}

/// A string literal PostgreSQL reads back as `value`, whatever
/// `standard_conforming_strings` says.
pub(crate) fn literal(value: &str) -> String {
    let quoted = value.replace('\'', "''");
    if value.contains('\\') {
        format!("E'{}'", quoted.replace('\\', "\\\\"))
    } else {
        format!("'{quoted}'")
    }
}

/// Writes `params` into `sql` as literals for its `$1`, `$2`, ….
pub(crate) fn with_params(sql: &str, params: &[&str]) -> String {
    if params.is_empty() {
        return sql.to_owned();
    }
    let tokens = pythia_plsql::tokenize(sql);
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for pair in tokens.windows(2) {
        let (dollar, number) = (&pair[0], &pair[1]);
        if &sql[dollar.span.clone()] != "$"
            || number.kind != pythia_plsql::TokenKind::Number
            || number.span.start != dollar.span.end
        {
            continue;
        }
        let Some(value) = sql[number.span.clone()]
            .parse::<usize>()
            .ok()
            .and_then(|n| params.get(n.checked_sub(1)?))
        else {
            continue;
        };
        out.push_str(&sql[last..dollar.span.start]);
        out.push_str(&literal(value));
        last = number.span.end;
    }
    out.push_str(&sql[last..]);
    out
}

/// Whether a statement may return rows (its columns are described first).
fn returns_rows(sql: &str) -> bool {
    let words = leading_words(sql);
    matches!(
        words.first().map(String::as_str),
        Some("SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "EXPLAIN" | "FETCH" | "CALL")
    ) || has_word(sql, &["RETURNING"])
}

fn is_dml(kind: &str) -> bool {
    matches!(kind, "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY")
}

fn column(name: &str, ty: Option<&Type>) -> Column {
    let numeric = ty.is_some_and(|t| {
        matches!(
            *t,
            Type::INT2
                | Type::INT4
                | Type::INT8
                | Type::FLOAT4
                | Type::FLOAT8
                | Type::NUMERIC
                | Type::OID
                | Type::MONEY
        )
    });
    Column {
        name: name.to_owned(),
        type_name: ty.map_or_else(|| "text".into(), type_name),
        numeric,
    }
}

/// The type the way PostgreSQL writes it in DDL.
fn type_name(ty: &Type) -> String {
    if let Kind::Array(inner) = ty.kind() {
        return format!("{}[]", type_name(inner));
    }
    match *ty {
        Type::INT2 => "smallint",
        Type::INT4 => "integer",
        Type::INT8 => "bigint",
        Type::FLOAT4 => "real",
        Type::FLOAT8 => "double precision",
        Type::BOOL => "boolean",
        Type::BPCHAR => "character",
        Type::VARCHAR => "character varying",
        Type::TIMESTAMPTZ => "timestamp with time zone",
        Type::TIMETZ => "time with time zone",
        _ => ty.name(),
    }
    .to_owned()
}

/// The 1-based character position of an error in the statement.
fn error_position(e: &tokio_postgres::Error) -> Option<usize> {
    match e.as_db_error()?.position()? {
        ErrorPosition::Original(position) => usize::try_from(*position).ok(),
        ErrorPosition::Internal { .. } => None,
    }
}

/// What a statement does to the session's transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxEffect {
    /// `BEGIN`, `START TRANSACTION`.
    Begins,
    /// Changes or locks rows.
    Changes,
    /// `COMMIT`, `ROLLBACK`, `END`, `ABORT`, `PREPARE TRANSACTION`.
    Ends,
    /// The session's own `SAVEPOINT`, `RELEASE`, `ROLLBACK TO`.
    Savepoint,
    None,
}

impl TxEffect {
    fn of(sql: &str) -> Self {
        let words = leading_words(sql);
        let first = words.first().map(String::as_str).unwrap_or_default();
        let second = words.get(1).map(String::as_str).unwrap_or_default();
        match first {
            "ROLLBACK" if second == "TO" => TxEffect::Savepoint,
            "COMMIT" | "ROLLBACK" | "END" | "ABORT" => TxEffect::Ends,
            "PREPARE" if second == "TRANSACTION" => TxEffect::Ends,
            "BEGIN" | "START" => TxEffect::Begins,
            "SAVEPOINT" | "RELEASE" => TxEffect::Savepoint,
            "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY" | "CALL" | "DO" | "TRUNCATE"
            | "LOCK" => TxEffect::Changes,
            "SELECT" | "WITH" | "TABLE" => {
                // `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE`
                let locks = word_followed_by(sql, "FOR", &["UPDATE", "SHARE", "NO", "KEY"]);
                if locks
                    || first == "WITH" && has_word(sql, &["INSERT", "UPDATE", "DELETE", "MERGE"])
                {
                    TxEffect::Changes
                } else {
                    TxEffect::None
                }
            }
            _ => TxEffect::None,
        }
    }

    /// Whether the statement runs after a savepoint of its own inside a
    /// transaction (not those that end it or manage savepoints themselves).
    fn guarded(self) -> bool {
        matches!(self, TxEffect::Changes | TxEffect::None)
    }
}

/// Whether the session has a transaction to commit. It follows the
/// statements; without the session's autocommit, a change opens a
/// transaction first (see [`Worker::execute`]).
#[derive(Debug, Default)]
struct Transaction {
    pending: bool,
}

impl Transaction {
    /// Notes a statement that succeeded.
    fn after(&mut self, effect: TxEffect) {
        match effect {
            TxEffect::Begins => self.pending = true,
            TxEffect::Ends => self.pending = false,
            TxEffect::Changes | TxEffect::Savepoint | TxEffect::None => {}
        }
    }
}

/// Connection options from `host[:port][/database][?sslmode=…]`, a
/// `postgresql://` URL or `key=value` pairs. `sslmode` is libpq's: `prefer`
/// (the default) and `require` don't check the server's certificate,
/// `verify-ca` and `verify-full` do. Through SSH, the host is as the SSH
/// server sees it.
pub(crate) fn config(params: &ConnectParams) -> Result<(Config, MakeRustlsConnect), DbError> {
    let text = params.connect_string.trim();
    let mut ssl = None;
    let spec = if let Some(pairs) = conninfo_pairs(text) {
        // `sslmode` taken out (the last one counts, as in libpq), the
        // rest as written: a quoted value keeps its spaces.
        let mut spec = text.to_owned();
        for (range, key, value) in pairs.iter().rev() {
            if key == "sslmode" {
                ssl.get_or_insert_with(|| value.to_ascii_lowercase());
                spec.replace_range(range.clone(), "");
            }
        }
        spec
    } else {
        let mut take_ssl = |option: &str| match option.strip_prefix("sslmode=") {
            Some(mode) => {
                ssl = Some(mode.to_ascii_lowercase());
                false
            }
            None => true,
        };
        let (address, query) = text.split_once('?').unwrap_or((text, ""));
        let options: Vec<&str> = query
            .split('&')
            .filter(|o| !o.is_empty() && take_ssl(o))
            .collect();
        let address = if address.contains("://") {
            address.to_owned()
        } else {
            format!("postgresql://{}", address.trim_start_matches('/'))
        };
        if options.is_empty() {
            address
        } else {
            format!("{address}?{}", options.join("&"))
        }
    };
    let mut config: Config = spec
        .parse()
        .map_err(|e| DbError::new(format!("Can't read the connect string: {e}")))?;
    if !params.username.is_empty() {
        config.user(&params.username);
    }
    if !params.password.is_empty() {
        config.password(&params.password);
    }
    if config.get_application_name().is_none() {
        config.application_name("Pythia");
    }
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(Duration::from_secs(15));
    }
    let (mode, verify) = match ssl.as_deref() {
        // As the driver read the string, if it found a mode Pythia didn't.
        None => (config.get_ssl_mode(), false),
        Some("prefer" | "allow") => (SslMode::Prefer, false),
        Some("disable") => (SslMode::Disable, false),
        Some("require") => (SslMode::Require, false),
        Some("verify-ca" | "verify-full") => (SslMode::Require, true),
        Some(other) => {
            return Err(DbError::new(format!(
                "Unknown sslmode {other} (use disable, prefer, require, verify-full)"
            )));
        }
    };
    config.ssl_mode(mode);
    if let Some(tunnel) = &params.ssh {
        config = through(&config, tunnel)?;
    }
    Ok((config, tls(verify)))
}

/// libpq's `key = value` pairs, when all of `text` is such pairs with
/// words for keys: each with where it is in `text`, its key and its value.
/// A value may be quoted (`'a b'`), and `\` takes the next character as it
/// is, as libpq reads them.
fn conninfo_pairs(text: &str) -> Option<Vec<(Range<usize>, String, String)>> {
    let mut chars = text.char_indices().peekable();
    let mut pairs = Vec::new();
    let skip_spaces = |chars: &mut std::iter::Peekable<std::str::CharIndices>| {
        while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
    };
    loop {
        skip_spaces(&mut chars);
        let Some(&(start, _)) = chars.peek() else {
            break;
        };
        let mut key = String::new();
        while let Some((_, c)) = chars.next_if(|(_, c)| !c.is_whitespace() && *c != '=') {
            key.push(c);
        }
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        skip_spaces(&mut chars);
        chars.next_if(|(_, c)| *c == '=')?;
        skip_spaces(&mut chars);
        let mut value = String::new();
        let mut end = text.len();
        if chars.next_if(|(_, c)| *c == '\'').is_some() {
            loop {
                match chars.next()? {
                    (_, '\\') => value.push(chars.next()?.1),
                    (i, '\'') => {
                        end = i + 1;
                        break;
                    }
                    (_, c) => value.push(c),
                }
            }
        } else {
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next().map(|(_, c)| c)),
                    c if c.is_whitespace() => {
                        end = i;
                        break;
                    }
                    c => value.push(c),
                }
            }
        }
        pairs.push((start..end, key, value));
    }
    (!pairs.is_empty()).then_some(pairs)
}

/// `config` through an SSH tunnel: TCP goes to the local end of a forward
/// to the database (`hostaddr`), TLS still checks the database host's
/// name. A config only adds hosts and ports, so it's built again.
fn through(config: &Config, tunnel: &Tunnel) -> Result<Config, DbError> {
    let host = match config.get_hosts() {
        [] => "localhost".to_owned(),
        [Host::Tcp(host)] => host.clone(),
        _ => {
            return Err(DbError::new(
                "Through SSH, connect to one host by name or address, as the SSH server sees it",
            ));
        }
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let local = tunnel.forward(&host, port)?;
    let mut through = Config::new();
    through.host(&host).hostaddr(local.ip()).port(local.port());
    if let Some(user) = config.get_user() {
        through.user(user);
    }
    if let Some(password) = config.get_password() {
        through.password(password);
    }
    if let Some(dbname) = config.get_dbname() {
        through.dbname(dbname);
    }
    if let Some(options) = config.get_options() {
        through.options(options);
    }
    if let Some(name) = config.get_application_name() {
        through.application_name(name);
    }
    if let Some(timeout) = config.get_connect_timeout() {
        through.connect_timeout(*timeout);
    }
    through
        .ssl_mode(config.get_ssl_mode())
        .ssl_negotiation(config.get_ssl_negotiation())
        .channel_binding(config.get_channel_binding())
        .target_session_attrs(config.get_target_session_attrs());
    Ok(through)
}

fn tls(verify: bool) -> MakeRustlsConnect {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions");
    let config = if verify {
        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let native = rustls_native_certs::load_native_certs();
        roots.add_parsable_certificates(native.certs);
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth()
    };
    MakeRustlsConnect::new(config)
}

/// Accepts the server's certificate unchecked, as libpq does below
/// `sslmode=verify-ca` (the connection is still encrypted).
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl From<tokio_postgres::Error> for DbError {
    fn from(e: tokio_postgres::Error) -> Self {
        let lost = e.is_closed()
            || std::error::Error::source(&e).is_some_and(|s| s.is::<std::io::Error>());
        let Some(db) = e.as_db_error() else {
            return Self {
                lost,
                ..Self::new(format!("PostgreSQL: {e}"))
            };
        };
        let mut message = format!("{} {}: {}", db.severity(), db.code().code(), db.message());
        if let Some(detail) = db.detail() {
            message.push_str(&format!("\nDETAIL: {detail}"));
        }
        if let Some(hint) = db.hint() {
            message.push_str(&format!("\nHINT: {hint}"));
        }
        if let Some(context) = db.where_() {
            message.push_str(&format!("\nCONTEXT: {context}"));
        }
        // Terminated session, server shutting down.
        let gone = [
            SqlState::ADMIN_SHUTDOWN,
            SqlState::CRASH_SHUTDOWN,
            SqlState::CANNOT_CONNECT_NOW,
        ]
        .contains(db.code());
        Self {
            lost: lost || gone,
            ..Self::new(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(connect_string: &str) -> ConnectParams {
        ConnectParams {
            kind: crate::DbKind::Postgres,
            username: "app".into(),
            password: "secret".into(),
            connect_string: connect_string.into(),
            privilege: Default::default(),
            init_sql: Vec::new(),
            ssh: None,
        }
    }

    /// The statements after which the session reads its search path again
    /// (#320): set_config anywhere in one too (#362), not in a string or a
    /// comment.
    #[test]
    fn statements_that_may_change_the_path() {
        for sql in [
            "set search_path = a",
            "commit",
            "prepare transaction 't'",
            "create schema s",
            "select set_config('search_path', 'a', false)",
            "select 1, pg_catalog.Set_Config('role', 'r', false)",
        ] {
            assert!(changes_path(sql), "{sql}");
        }
        for sql in [
            "select 1",
            "select 'set_config(' as t -- set_config(",
            "prepare p as select 1",
            "create table t (a int)",
        ] {
            assert!(!changes_path(sql), "{sql}");
        }
    }

    #[test]
    fn connect_strings() {
        let parse = |text: &str| config(&params(text)).map(|(c, _)| c);
        let c = parse("db.example.com:5433/shop?sslmode=verify-full").unwrap();
        assert_eq!(c.get_ports(), [5433]);
        assert_eq!(c.get_dbname(), Some("shop"));
        assert_eq!(c.get_user(), Some("app"));
        assert_eq!(c.get_ssl_mode(), SslMode::Require);
        let c = parse("localhost").unwrap();
        assert_eq!(c.get_ssl_mode(), SslMode::Prefer);
        assert_eq!(c.get_application_name(), Some("Pythia"));
        let c = parse("host=h port=6000 dbname=x sslmode=disable").unwrap();
        assert_eq!(
            (c.get_ports(), c.get_ssl_mode()),
            (&[6000][..], SslMode::Disable)
        );
        let c = parse("postgresql://u@[::1]/d?sslmode=require").unwrap();
        assert_eq!(c.get_dbname(), Some("d"));
        assert_eq!(c.get_user(), Some("app"));
        assert!(parse("h?sslmode=sometimes").is_err());
        // Pairs as libpq reads them: a `?` in a value doesn't make a URL,
        // a quoted value keeps its spaces, spaces may surround `=`.
        let c = parse("host=h user=u password='Pa?s  s' dbname=shop").unwrap();
        assert_eq!(c.get_password(), Some(&b"secret"[..]));
        assert_eq!(c.get_dbname(), Some("shop"));
        let mut quoted = params("host=h password='a  b\\'c' dbname=x");
        quoted.password.clear();
        let c = config(&quoted).unwrap().0;
        assert_eq!(c.get_password(), Some(&b"a  b'c"[..]));
        let c = parse("host = h sslmode = require dbname = x").unwrap();
        assert_eq!(c.get_ssl_mode(), SslMode::Require);
        assert_eq!(c.get_dbname(), Some("x"));
        let c = parse("host=h sslmode='verify-full'").unwrap();
        assert_eq!(c.get_ssl_mode(), SslMode::Require);
        // A mode the driver read itself isn't weakened to prefer.
        let c = parse("postgresql://u:pa?ss@h/d?sslmode=require").unwrap();
        assert_eq!(c.get_ssl_mode(), SslMode::Require);
        assert_eq!(
            conninfo_pairs("a=1  b = 'x y' c=\\ d"),
            Some(vec![
                (0..3, "a".into(), "1".into()),
                (5..14, "b".into(), "x y".into()),
                (15..20, "c".into(), " d".into()),
            ])
        );
        for text in [
            "localhost",
            "h:5432/db?sslmode=require",
            "a=1 b",
            "a='x",
            "=1",
        ] {
            assert_eq!(conninfo_pairs(text), None, "{text}");
        }
    }

    #[test]
    fn literals_and_params() {
        assert_eq!(literal("it's"), "'it''s'");
        assert_eq!(literal(r"a\b"), r"E'a\\b'");
        assert_eq!(
            with_params(
                "select $1, '$2', $2::int, $3 from t where a = $1",
                &["x", "7"]
            ),
            "select 'x', '$2', '7'::int, $3 from t where a = 'x'"
        );
    }

    #[test]
    fn transaction_effects() {
        let cases = [
            ("select 1", TxEffect::None),
            ("select * from t for update", TxEffect::Changes),
            ("select * from t for no key update", TxEffect::Changes),
            ("select 'for update' from t", TxEffect::None),
            (
                "with d as (delete from t returning *) select * from d",
                TxEffect::Changes,
            ),
            ("insert into t values (1)", TxEffect::Changes),
            ("do $$ begin null; end $$", TxEffect::Changes),
            ("create table x (a int)", TxEffect::None),
            ("savepoint a", TxEffect::Savepoint),
            ("rollback to savepoint a", TxEffect::Savepoint),
            ("release a", TxEffect::Savepoint),
            ("rollback", TxEffect::Ends),
            ("end", TxEffect::Ends),
            ("begin", TxEffect::Begins),
            ("start transaction read only", TxEffect::Begins),
        ];
        for (sql, effect) in cases {
            assert_eq!(TxEffect::of(sql), effect, "{sql}");
        }
    }

    #[test]
    fn type_names() {
        assert_eq!(type_name(&Type::INT4_ARRAY), "integer[]");
        assert_eq!(type_name(&Type::NUMERIC), "numeric");
        assert!(column("n", Some(&Type::NUMERIC)).numeric);
        assert!(!column("t", None).numeric);
    }
}
