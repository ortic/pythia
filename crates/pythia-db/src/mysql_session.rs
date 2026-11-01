//! MySQL sessions: the requests and events of an Oracle session, served by
//! the pure Rust `mysql` driver.
//!
//! A MySQL connection can't do anything else while a result set is being
//! read. So a query's rows are fetched while the worker waits for
//! `FetchMore`s of that query; any other request closes the result first,
//! stopping the server with `KILL QUERY` when rows are left.
//!
//! Transactions behave like Oracle's: queries always see what others have
//! committed, and changes wait for Commit unless the session autocommits.
//! The server's autocommit stays on (with it off, InnoDB would keep the
//! first query's snapshot and its metadata locks until the next commit);
//! a change opens a transaction of its own instead.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use mysql::consts::{CapabilityFlags, ColumnFlags, ColumnType};
use mysql::prelude::{Protocol, Queryable};
use mysql::{Conn, Opts, OptsBuilder, Params, QueryResult, SslOpts, Value};
use mysql_common::proto::codec::error::PacketCodecError;
use pythia_plsql::Syntax;
use tracing::{debug, info, warn};

use crate::dialect::Dialect;
use crate::meta::{MetaQuery, MetaResult};
use crate::session::{CancelSlot, Canceller, Events};
use crate::sql_text::{
    WHOLE_BATCH_BYTES, float_text, hex, leading_words_in, release, replace_binds, row_bytes,
    statement_kind_in,
};
use crate::types::{
    Cell, Client, Column, ConnectParams, DbError, DbEvent, DbRequest, ExecSummary, ServerInfo,
};

const DEFAULT_PORT: u16 = 3306;

/// How long `KILL QUERY` waits for each answer of the server.
const KILL_WAIT: Duration = Duration::from_secs(10);

pub(crate) fn run(
    params: ConnectParams,
    requests: Receiver<DbRequest>,
    events: Events,
    shared: CancelSlot,
) {
    info!(?params, "connecting");
    let connected = options(&params)
        .and_then(|opts| connect(&opts).map_err(|e| params.explain(connect_error(e))));
    let (opts, conn) = match connected {
        Ok(connected) => connected,
        Err(error) => {
            warn!("connect failed: {error}");
            events.send(DbEvent::ConnectFailed(error));
            return;
        }
    };
    let connection_id = conn.connection_id();
    let kill: Canceller = Arc::new(move || kill_query(&opts, connection_id));
    *shared.lock().expect("cancel mutex") = Some(kill.clone());

    let mut worker = Worker {
        conn,
        events,
        kill,
        autocommit: false,
        tx: Transaction::default(),
        time_zone: None,
    };
    match worker.on_connect(&params.init_sql) {
        Ok((server, failed)) => {
            worker.events.send(DbEvent::Connected(server));
            for notice in failed {
                worker.events.send(DbEvent::Notice(notice));
            }
            worker.report_time_zone();
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
                let result = crate::mysql_meta::run(&mut worker.conn, &query);
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
                result: Err(DbError::unsupported("This tool", Dialect::MYSQL)),
            }),
            DbRequest::Run { id, task } => {
                let started = Instant::now();
                let result = task(&mut MySqlClient(&mut worker.conn));
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
    // Dropping the connection rolls back what wasn't committed.
    drop(worker.conn);
    info!("disconnected");
    worker.events.send(DbEvent::Disconnected);
}

struct Worker {
    conn: Conn,
    events: Events,
    /// Stops the statement running on `conn`.
    kill: Canceller,
    autocommit: bool,
    tx: Transaction,
    /// The time zone last reported (see [`Worker::report_time_zone`]).
    time_zone: Option<String>,
}

/// How a statement ended: a query's rows were reported as they were
/// fetched (`ended` when all of them were read), anything else is
/// summarized.
enum Outcome {
    Query { ended: bool },
    Done { affected: u64, warnings: u16 },
}

impl Worker {
    /// Sets the session up and runs the login statements, which may pick
    /// the database (#103): what the server reports is read after them.
    /// Their failures come back as notices.
    fn on_connect(&mut self, init_sql: &[String]) -> Result<(ServerInfo, Vec<String>), DbError> {
        // Whatever the server's default: see `Transaction`.
        self.conn.query_drop("set autocommit = 1")?;
        let mut failed = Vec::new();
        for sql in init_sql {
            if let Err(e) = self.conn.query_drop(sql) {
                let e = DbError::from(e);
                failed.push(format!(
                    "Login script: {e} ({})",
                    sql.lines().next().unwrap_or_default()
                ));
            }
        }
        let (version, comment, schema): (String, Option<String>, Option<String>) = self
            .conn
            .query_first("select version(), @@version_comment, database()")?
            .ok_or_else(|| DbError::new("The server didn't report its version"))?;
        let server = ServerInfo {
            dialect: Dialect::MYSQL,
            release: release(&version),
            banner: match comment {
                Some(comment) => format!("{comment} {version}"),
                None => version.clone(),
            },
            version,
            current_schema: schema.unwrap_or_default(),
        };
        Ok((server, failed))
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
        let effect = TxEffect::of(sql);
        // Changes wait for Commit: they get a transaction of their own.
        let opened = !self.autocommit && !self.tx.pending && effect.changes();
        if opened {
            if let Err(e) = self.conn.query_drop("start transaction") {
                self.events.send(DbEvent::Failed {
                    id,
                    error: e.into(),
                    elapsed: started.elapsed(),
                });
                return None;
            }
            self.tx.pending = true;
        }
        let fetch = Fetch {
            id,
            max_rows,
            whole,
            started,
            events: &self.events,
            requests,
            kill: &self.kill,
        };
        let ran = Self::run_statement(&mut self.conn, sql, binds, fetch, &mut next);
        match ran {
            Ok(outcome) => {
                self.tx.after(effect);
                if let Outcome::Done { affected, warnings } = outcome {
                    let kind = statement_kind_in(sql, Syntax::MySql);
                    let summary = ExecSummary {
                        rows_affected: is_dml(&kind).then_some(affected),
                        kind,
                        compiled: None,
                    };
                    self.events.send(DbEvent::Executed {
                        id,
                        summary,
                        elapsed: started.elapsed(),
                    });
                    if warnings > 0 {
                        self.report_warnings();
                    }
                } else if let Outcome::Query { ended: true } = outcome
                    // A query's are known once all its rows are read (#210).
                    && self.conn.warnings() > 0
                {
                    self.report_warnings();
                }
            }
            Err(e) => {
                let mut error = DbError::from(e);
                error.offset = error_offset(sql, &error.message);
                // Nothing to commit from a failed change of its own; a
                // deadlock rolls the whole transaction back anyway.
                if opened || error.code == Some(1213) {
                    let _ = self.conn.query_drop("rollback");
                    self.tx.pending = false;
                }
                self.events.send(DbEvent::Failed {
                    id,
                    error,
                    elapsed: started.elapsed(),
                });
            }
        }
        // SET time_zone, or a procedure that sets it: the first words tell,
        // also of an INSERT of megabytes, and of mysqldump's /*!40103 SET
        // TIME_ZONE='+00:00' */ (#219).
        let head = sql.char_indices().nth(1000).map_or(sql, |(i, _)| &sql[..i]);
        if matches!(
            leading_words_in(head, Syntax::MySql)
                .first()
                .map(String::as_str),
            Some("SET" | "CALL")
        ) {
            self.report_time_zone();
        }
        self.report_transaction();
        next
    }

    fn run_statement(
        conn: &mut Conn,
        sql: &str,
        binds: &[(String, Option<String>)],
        fetch: Fetch<'_>,
        next: &mut Option<DbRequest>,
    ) -> Result<Outcome, mysql::Error> {
        if binds.is_empty() {
            let result = conn.query_iter(sql)?;
            fetch.stream(result, next)
        } else {
            let (sql, values) = positional(sql, binds);
            let result = conn.exec_iter(sql, Params::Positional(values))?;
            fetch.stream(result, next)
        }
    }

    fn end_transaction(&mut self, commit: bool) {
        let result = self
            .conn
            .query_drop(if commit { "commit" } else { "rollback" });
        match result {
            Ok(()) => {
                self.tx.pending = false;
                self.events.send(DbEvent::Notice(
                    if commit { "Committed" } else { "Rolled back" }.into(),
                ));
            }
            Err(e) => self.events.send(DbEvent::Error(e.into())),
        }
        self.report_transaction();
    }

    fn report_transaction(&self) {
        self.events.send(DbEvent::Transaction {
            pending: self.tx.pending,
        });
    }

    /// Reports the zone the session shows TIMESTAMP values in, when it
    /// changed (#213). `SYSTEM`, which another server reads as its own
    /// zone, as the server's offset when it has the same one all year (a
    /// server on UTC: `+00:00`).
    fn report_time_zone(&mut self) {
        // The session's zone, the server's offsets in January and July.
        type Zone = (String, Option<i64>, Option<i64>);
        let read: Result<Option<Zone>, _> = self.conn.query_first(
            "select @@session.time_zone, \
                    timestampdiff(minute, convert_tz(d.j, 'SYSTEM', '+00:00'), d.j), \
                    timestampdiff(minute, convert_tz(d.s, 'SYSTEM', '+00:00'), d.s) \
               from (select cast(concat(year(now()), '-01-15 12:00:00') as datetime) j, \
                            cast(concat(year(now()), '-07-15 12:00:00') as datetime) s) d",
        );
        let zone = match read {
            Ok(Some((zone, Some(january), Some(july))))
                if zone.eq_ignore_ascii_case("SYSTEM") && january == july =>
            {
                let sign = if january < 0 { '-' } else { '+' };
                format!("{sign}{:02}:{:02}", january.abs() / 60, january.abs() % 60)
            }
            Ok(Some((zone, ..))) => zone,
            Ok(None) => return,
            Err(e) => {
                warn!("reading the time zone: {e}");
                return;
            }
        };
        if self.time_zone.as_ref() != Some(&zone) {
            self.events.send(DbEvent::TimeZone(zone.clone()));
            self.time_zone = Some(zone);
        }
    }

    /// The statement's warnings, as notices (MySQL's counterpart of
    /// messages printed by the database).
    fn report_warnings(&mut self) {
        let warnings: Result<Vec<(String, u32, String)>, _> = self.conn.query("show warnings");
        match warnings {
            Ok(warnings) => {
                for (level, code, message) in warnings {
                    self.events
                        .send(DbEvent::Notice(format!("{level} {code}: {message}")));
                }
            }
            Err(e) => warn!("reading warnings: {e}"),
        }
    }
}

/// Reads a statement's result for [`Worker::run_statement`].
struct Fetch<'a> {
    id: u64,
    max_rows: usize,
    /// Binary values whole, for a file (#204).
    whole: bool,
    started: Instant,
    events: &'a Events,
    requests: &'a Receiver<DbRequest>,
    kill: &'a Canceller,
}

impl Fetch<'_> {
    /// Reports a statement's results. Each set of rows is one, fetched in
    /// batches: a CALL's SELECTs, the statements of one text (#290); the
    /// next comes once all rows of the one before were read, announced by
    /// [`DbEvent::MoreResults`] before its last rows. Sets without rows (a
    /// DO, an INSERT, a CALL's own status) are counted as done.
    fn stream<T: Protocol>(
        self,
        mut result: QueryResult<'_, '_, '_, T>,
        next: &mut Option<DbRequest>,
    ) -> Result<Outcome, mysql::Error> {
        let mut done = (0, 0);
        let Some(mut types) = rows_ahead(&mut result, &mut done)? else {
            return Ok(Outcome::Done {
                affected: done.0,
                warnings: done.1,
            });
        };
        let id = self.id;
        let mut started = self.started;
        loop {
            let columns = types.iter().map(column).collect();
            self.events.send(DbEvent::QueryStarted { id, columns });
            let mut max_rows = self.max_rows;
            let rows = loop {
                let mut rows = Vec::with_capacity(max_rows.min(10_000));
                let mut more = true;
                let mut bytes = 0;
                while rows.len() < max_rows && bytes < WHOLE_BATCH_BYTES {
                    match result.next() {
                        Some(Ok(row)) => {
                            let cells: Vec<Cell> = row
                                .unwrap()
                                .iter()
                                .zip(&types)
                                .map(|(value, ty)| cell(value, ty, self.whole))
                                .collect();
                            if self.whole {
                                bytes += row_bytes(&cells);
                            }
                            rows.push(cells);
                        }
                        Some(Err(e)) => {
                            let mut error = DbError::from(e);
                            if error.code == Some(1317) {
                                error.message = "Cancelled while fetching".into();
                            }
                            self.events.send(DbEvent::Failed {
                                id,
                                error,
                                elapsed: started.elapsed(),
                            });
                            return Ok(Outcome::Query { ended: false });
                        }
                        None => {
                            more = false;
                            break;
                        }
                    }
                }
                if !more {
                    break rows;
                }
                self.events.send(DbEvent::Rows {
                    id,
                    rows,
                    more,
                    elapsed: started.elapsed(),
                });
                match self.requests.recv() {
                    Ok(DbRequest::FetchMore {
                        id: wanted,
                        max_rows: n,
                    }) if wanted == id => {
                        max_rows = n;
                        started = Instant::now();
                    }
                    other => {
                        // Rows are left: stop the server instead of reading
                        // them all when the result is dropped on return.
                        (self.kill)();
                        *next = Some(other.unwrap_or(DbRequest::Disconnect));
                        return Ok(Outcome::Query { ended: false });
                    }
                }
            };
            let last = DbEvent::Rows {
                id,
                rows,
                more: false,
                elapsed: started.elapsed(),
            };
            // A file has one result: the others are read, and said so.
            if self.whole {
                self.events.send(last);
                self.drop_rest(&mut result)?;
                return Ok(Outcome::Query { ended: true });
            }
            let after = rows_ahead(&mut result, &mut done);
            if !matches!(after, Ok(None)) {
                self.events.send(DbEvent::MoreResults { id });
            }
            self.events.send(last);
            match after? {
                Some(next_types) => {
                    types = next_types;
                    started = Instant::now();
                }
                None => return Ok(Outcome::Query { ended: true }),
            }
        }
    }

    /// Reads the statement's sets after the one written to a file, and
    /// says how many results it didn't write.
    fn drop_rest<T: Protocol>(
        &self,
        result: &mut QueryResult<'_, '_, '_, T>,
    ) -> Result<(), mysql::Error> {
        let mut sets = 0;
        while rows_ahead(result, &mut (0, 0))?.is_some() {
            sets += 1;
            for row in result.by_ref() {
                row?;
            }
        }
        if sets > 0 {
            self.events.send(DbEvent::Notice(format!(
                "The statement returned {sets} more result(s); only the first was written to \
                 the file"
            )));
        }
        Ok(())
    }
}

/// Moves `result` past the sets without rows to the next one with them,
/// and returns its columns; `None` at the end. A set without rows stands
/// for a statement that ran (its rows affected and warnings are added to
/// `done`) or one that failed, whose error it returns.
fn rows_ahead<T: Protocol>(
    result: &mut QueryResult<'_, '_, '_, T>,
    done: &mut (u64, u16),
) -> Result<Option<Vec<mysql::Column>>, mysql::Error> {
    loop {
        let types: Vec<mysql::Column> = result.columns().as_ref().to_vec();
        if !types.is_empty() {
            return Ok(Some(types));
        }
        // Not a set of rows: iterating it moves on to the next set, and
        // reading it takes nothing from that one.
        let Some(mut set) = result.iter() else {
            return Ok(None);
        };
        done.0 += set.affected_rows();
        done.1 = set.warnings();
        if let Some(Err(e)) = set.next() {
            return Err(e);
        }
    }
}

/// A MySQL connection as a [`Client`].
struct MySqlClient<'a>(&'a mut Conn);

impl Client for MySqlClient<'_> {
    fn dialect(&self) -> Dialect {
        Dialect::MYSQL
    }

    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Cell>>, DbError> {
        if params.is_empty() {
            Ok(all_rows(self.0.query_iter(sql)?)?)
        } else {
            let params = params.iter().map(|p| Value::from(*p)).collect();
            Ok(all_rows(
                self.0.exec_iter(sql, Params::Positional(params))?,
            )?)
        }
    }

    fn meta(&mut self, query: &MetaQuery) -> Result<MetaResult, DbError> {
        crate::mysql_meta::run(self.0, query)
    }
}

/// The rows of a result's first set as text.
pub(crate) fn all_rows<T: Protocol>(
    mut result: QueryResult<'_, '_, '_, T>,
) -> Result<Vec<Vec<Cell>>, mysql::Error> {
    let types: Vec<mysql::Column> = result.columns().as_ref().to_vec();
    let mut rows = Vec::new();
    for row in result.by_ref() {
        let values = row?.unwrap();
        rows.push(
            values
                .iter()
                .zip(&types)
                .map(|(v, c)| cell(v, c, false))
                .collect(),
        );
    }
    Ok(rows)
}

/// Connection options from `host[:port][/database][?tls[=skip-verify]]`
/// (a `mysql://` prefix is allowed). Through SSH, the host is as the SSH
/// server sees it.
pub(crate) fn options(params: &ConnectParams) -> Result<Opts, DbError> {
    let target = parse_target(&params.connect_string)?;
    let (host, port) = match &params.ssh {
        Some(tunnel) => {
            let local = tunnel.forward(&target.host, target.port)?;
            (local.ip().to_string(), local.port())
        }
        None => (target.host, target.port),
    };
    let mut builder = OptsBuilder::new()
        .ip_or_hostname(Some(host))
        .tcp_port(port)
        .user(Some(&params.username))
        .pass(Some(&params.password))
        .db_name(target.database)
        .prefer_socket(false)
        .tcp_connect_timeout(Some(Duration::from_secs(15)))
        // Rows affected are the rows matched, as on Oracle, not only those
        // whose values changed: editing a result checks that each UPDATE
        // found its row, even when it sets a value the row already has.
        .additional_capabilities(CapabilityFlags::CLIENT_FOUND_ROWS);
    match target.tls {
        Tls::Off => {}
        // Through SSH the driver connects to 127.0.0.1: the certificate is
        // still checked, its host name can't be.
        Tls::Verify => {
            builder = builder.ssl_opts(
                SslOpts::default().with_danger_skip_domain_validation(params.ssh.is_some()),
            )
        }
        Tls::SkipVerify => {
            builder = builder.ssl_opts(
                SslOpts::default()
                    .with_danger_accept_invalid_certs(true)
                    .with_danger_skip_domain_validation(true),
            )
        }
    }
    Ok(builder.into())
}

#[derive(Debug, PartialEq, Eq)]
enum Tls {
    Off,
    Verify,
    SkipVerify,
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
    database: Option<String>,
    tls: Tls,
}

fn parse_target(text: &str) -> Result<Target, DbError> {
    let text = text.trim();
    let text = text.strip_prefix("mysql://").unwrap_or(text);
    let (address, query) = text.split_once('?').unwrap_or((text, ""));
    let (host_port, database) = match address.split_once('/') {
        Some((host_port, db)) => (host_port, Some(db.trim()).filter(|d| !d.is_empty())),
        None => (address, None),
    };
    // `[::1]:3306`, `host:3306` or just the host.
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
    let port = match port {
        Some(port) => port
            .parse()
            .map_err(|_| DbError::new(format!("Not a port number: {port}")))?,
        None => DEFAULT_PORT,
    };
    let host = if host.is_empty() { "localhost" } else { host };
    let mut tls = Tls::Off;
    for option in query.split('&').filter(|o| !o.is_empty()) {
        tls = match option.to_ascii_lowercase().as_str() {
            "tls" | "ssl" | "tls=verify" | "ssl=verify" => Tls::Verify,
            "tls=skip-verify" | "ssl=skip-verify" => Tls::SkipVerify,
            "tls=off" | "ssl=off" => Tls::Off,
            _ => {
                return Err(DbError::new(format!(
                    "Unknown option {option} (use tls, tls=skip-verify)"
                )));
            }
        };
    }
    Ok(Target {
        host: host.to_owned(),
        port,
        database: database.map(str::to_owned),
        tls,
    })
}

/// Connects as `opts` say; returns the options that connected, which
/// `KILL QUERY` connects with too. The driver tries only the first of the
/// host's addresses (#239): `localhost` is `::1` first on most
/// systems, where MySQL often listens on 127.0.0.1 only, so connecting
/// failed with "Connection refused". When that first address refuses or
/// doesn't answer, the others are tried in turn: by their IP, or, where TLS
/// checks the host's name, by the name bound to the address's family, whose
/// addresses the driver then tries first.
fn connect(opts: &Opts) -> Result<(Opts, Conn), mysql::Error> {
    // No TCP connection was made: nothing was sent, no login failed.
    let unreachable = |e: &mysql::Error| {
        matches!(
            e,
            mysql::Error::DriverError(
                mysql::DriverError::ConnectTimeout | mysql::DriverError::CouldNotConnect(Some(_))
            )
        )
    };
    let error = match Conn::new(opts.clone()) {
        Ok(conn) => return Ok((opts.clone(), conn)),
        Err(e) => e,
    };
    let host = opts.get_ip_or_hostname().into_owned();
    let addresses: Vec<SocketAddr> = match (host.as_str(), opts.get_tcp_port()).to_socket_addrs() {
        Ok(addresses) if unreachable(&error) => addresses.collect(),
        _ => return Err(error),
    };
    let by_name = host.parse::<IpAddr>().is_err()
        && opts
            .get_ssl_opts()
            .is_some_and(|tls| !tls.skip_domain_validation());
    let mut families = Vec::new();
    for address in addresses.iter().skip(1) {
        let builder = OptsBuilder::from_opts(opts.clone());
        let builder = if by_name {
            let any = match address {
                SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
                SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
            };
            if families.contains(&any) {
                continue;
            }
            families.push(any);
            builder.bind_address(Some(any))
        } else {
            builder.ip_or_hostname(Some(address.ip().to_string()))
        };
        let opts = Opts::from(builder);
        match Conn::new(opts.clone()) {
            Ok(conn) => return Ok((opts, conn)),
            Err(e) if unreachable(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Err(error)
}

/// A failed connect, with what to do about a certificate that isn't
/// trusted (#211): it read "Lost connection to the MySQL server: IO error:
/// `invalid peer certificate: UnknownIssuer'"; or about a server without
/// TLS (#222).
fn connect_error(e: mysql::Error) -> DbError {
    if let mysql::Error::DriverError(mysql::DriverError::TlsNotSupported) = e {
        return DbError::new(
            "The server doesn't offer TLS, which ?tls asks for. Connect without ?tls, or turn \
             TLS on at the server.",
        );
    }
    let certificate = match &e {
        mysql::Error::IoError(io) | mysql::Error::CodecError(PacketCodecError::Io(io)) => io
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>())
            .filter(|tls| matches!(tls, rustls::Error::InvalidCertificate(_))),
        _ => None,
    };
    match certificate {
        Some(certificate) => DbError::new(format!(
            "The server's certificate isn't trusted ({certificate}). For a self-signed one, \
             connect with ?tls=skip-verify, like localhost:3306/shop?tls=skip-verify."
        )),
        None => e.into(),
    }
}

/// Stops the statement running on connection `id` from a connection of its
/// own (MySQL's way to break into a session). A server that doesn't answer
/// holds it up for [`KILL_WAIT`] at most (#207).
fn kill_query(opts: &Opts, id: u32) {
    let opts = OptsBuilder::from_opts(opts.clone())
        .read_timeout(Some(KILL_WAIT))
        .write_timeout(Some(KILL_WAIT));
    let killed = Conn::new(opts).and_then(|mut conn| conn.query_drop(format!("kill query {id}")));
    if let Err(e) = killed {
        warn!("cancel failed: {e}");
    }
}

/// Replaces `:name` binds by `?` with their values in order.
fn positional(sql: &str, binds: &[(String, Option<String>)]) -> (String, Vec<Value>) {
    let mut values = Vec::new();
    let sql = replace_binds(sql, binds, |_, value| {
        values.push(match value {
            Some(v) => Value::Bytes(v.clone().into_bytes()),
            None => Value::NULL,
        });
        "?".into()
    });
    (sql, values)
}

pub(crate) fn column(c: &mysql::Column) -> Column {
    use ColumnType::*;
    let numeric = matches!(
        c.column_type(),
        MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
            | MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
    );
    Column {
        name: c.name_str().into_owned(),
        type_name: type_name(c),
        numeric,
    }
}

/// The column's type the way MySQL names it in DDL.
fn type_name(c: &mysql::Column) -> String {
    use ColumnType::*;
    let binary = is_binary(c);
    let flags = c.flags();
    let name = if flags.contains(ColumnFlags::ENUM_FLAG) {
        "ENUM"
    } else if flags.contains(ColumnFlags::SET_FLAG) {
        "SET"
    } else {
        match c.column_type() {
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "DECIMAL",
            MYSQL_TYPE_TINY => "TINYINT",
            MYSQL_TYPE_SHORT => "SMALLINT",
            MYSQL_TYPE_INT24 => "MEDIUMINT",
            MYSQL_TYPE_LONG => "INT",
            MYSQL_TYPE_LONGLONG => "BIGINT",
            MYSQL_TYPE_FLOAT => "FLOAT",
            MYSQL_TYPE_DOUBLE => "DOUBLE",
            MYSQL_TYPE_NULL => "NULL",
            MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "TIMESTAMP",
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "DATE",
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "TIME",
            MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "DATETIME",
            MYSQL_TYPE_YEAR => "YEAR",
            MYSQL_TYPE_BIT => "BIT",
            MYSQL_TYPE_JSON => "JSON",
            MYSQL_TYPE_VECTOR => "VECTOR",
            MYSQL_TYPE_GEOMETRY => "GEOMETRY",
            MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB => {
                if binary {
                    "BLOB"
                } else {
                    "TEXT"
                }
            }
            MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => {
                if binary {
                    "VARBINARY"
                } else {
                    "VARCHAR"
                }
            }
            MYSQL_TYPE_STRING => {
                if binary {
                    "BINARY"
                } else {
                    "CHAR"
                }
            }
            _ => "UNKNOWN",
        }
    };
    // Only numbers are unsigned: MariaDB flags a TIMESTAMP and an INET6
    // so too, MySQL a YEAR and a BIT (#221).
    let number = matches!(
        name,
        "DECIMAL" | "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" | "FLOAT" | "DOUBLE"
    );
    if flags.contains(ColumnFlags::UNSIGNED_FLAG) && number {
        format!("{name} UNSIGNED")
    } else {
        name.to_owned()
    }
}

/// Byte strings without a character set: shown as hex, like Oracle's RAW.
fn is_binary(c: &mysql::Column) -> bool {
    use ColumnType::*;
    const BINARY_CHARSET: u16 = 63;
    c.character_set() == BINARY_CHARSET
        && matches!(
            c.column_type(),
            MYSQL_TYPE_STRING
                | MYSQL_TYPE_VAR_STRING
                | MYSQL_TYPE_VARCHAR
                | MYSQL_TYPE_TINY_BLOB
                | MYSQL_TYPE_MEDIUM_BLOB
                | MYSQL_TYPE_LONG_BLOB
                | MYSQL_TYPE_BLOB
                | MYSQL_TYPE_BIT
                | MYSQL_TYPE_GEOMETRY
        )
}

/// A value as text; binary values cut short unless `whole`.
pub(crate) fn cell(value: &Value, column: &mysql::Column, whole: bool) -> Cell {
    Some(match value {
        Value::NULL => return None,
        Value::Bytes(bytes) if is_binary(column) => hex(bytes, whole),
        Value::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Int(v) => v.to_string(),
        Value::UInt(v) => v.to_string(),
        // Values of a statement with binds, which MySQL sends in its binary
        // protocol; without binds they come as its text (#261).
        Value::Float(v) => float_text(*v),
        Value::Double(v) => float_text(*v),
        Value::Date(y, m, d, h, mi, s, us) => {
            let date = format!("{y:04}-{m:02}-{d:02}");
            if matches!(
                column.column_type(),
                ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE
            ) {
                date
            } else if *us > 0 {
                format!("{date} {h:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{date} {h:02}:{mi:02}:{s:02}")
            }
        }
        Value::Time(negative, days, h, mi, s, us) => {
            let sign = if *negative { "-" } else { "" };
            let hours = days * 24 + u32::from(*h);
            if *us > 0 {
                format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{sign}{hours:02}:{mi:02}:{s:02}")
            }
        }
    })
}

fn is_dml(kind: &str) -> bool {
    matches!(kind, "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "LOAD")
}

/// What a statement does to the session's transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxEffect {
    /// `START TRANSACTION`, `BEGIN`.
    Begins,
    /// Changes or locks rows.
    Changes,
    /// `COMMIT`, `ROLLBACK`, or DDL (which commits implicitly).
    Ends,
    None,
}

impl TxEffect {
    /// By the first words, the code in `/*!…*/` too (#219): mysqldump's
    /// `/*!40000 ALTER TABLE … DISABLE KEYS */` commits.
    fn of(sql: &str) -> Self {
        let words = leading_words_in(sql, Syntax::MySql);
        let first = words.first().map(String::as_str).unwrap_or_default();
        let second = words.get(1).map(String::as_str).unwrap_or_default();
        match first {
            "COMMIT" => TxEffect::Ends,
            "ROLLBACK" if second != "TO" => TxEffect::Ends,
            "START" | "BEGIN" => TxEffect::Begins,
            "CREATE" if second == "TEMPORARY" => TxEffect::None,
            "CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE" | "GRANT" | "REVOKE" | "LOCK"
            | "UNLOCK" | "ANALYZE" | "OPTIMIZE" | "REPAIR" => TxEffect::Ends,
            "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "LOAD" | "CALL" => TxEffect::Changes,
            "SELECT" | "WITH" => {
                let upper = sql.to_ascii_uppercase();
                if upper.contains("FOR UPDATE")
                    || upper.contains("FOR SHARE")
                    || upper.contains("LOCK IN SHARE MODE")
                {
                    TxEffect::Changes
                } else {
                    TxEffect::None
                }
            }
            _ => TxEffect::None,
        }
    }

    fn changes(self) -> bool {
        self == TxEffect::Changes
    }
}

/// Whether the session has a transaction to commit. MySQL doesn't say
/// without extra privileges, so it follows the statements. The server's
/// autocommit stays on, so queries see what others committed and hold no
/// locks; without the session's autocommit, a change opens a transaction
/// first (see [`Worker::execute`]).
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
            // In autocommit mode a change outside a transaction is committed.
            TxEffect::Changes | TxEffect::None => {}
        }
    }
}

/// Where a syntax error is: MySQL quotes the statement from there on
/// (`... near 'frm emp' at line 1`).
fn error_offset(sql: &str, message: &str) -> Option<u32> {
    let start = message.find("near '")? + "near '".len();
    let end = message.rfind("' at line ")?;
    let near = message.get(start..end)?;
    let line: usize = message[end + "' at line ".len()..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let line_start = sql
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum::<usize>();
    if near.is_empty() {
        return u32::try_from(sql.trim_end().len()).ok();
    }
    let head = near.lines().next().unwrap_or(near);
    let found = sql.get(line_start..)?.find(head)?;
    u32::try_from(line_start + found).ok()
}

impl From<mysql::Error> for DbError {
    fn from(e: mysql::Error) -> Self {
        match e {
            mysql::Error::MySqlError(e) => Self {
                code: Some(i32::from(e.code)),
                // Killed connection, shutdown, idle timeout; a packet too
                // large, after which the server closes the connection.
                lost: matches!(e.code, 1053 | 1153 | 1927 | 4031),
                ..Self::new(format!("ERROR {} ({}): {}", e.code, e.state, e.message))
            },
            // The driver refuses to send a statement larger than the
            // server's max_allowed_packet, which it read at connect: the
            // connection is as it was (#205).
            mysql::Error::CodecError(PacketCodecError::PacketTooLarge)
            | mysql::Error::DriverError(mysql::DriverError::PacketTooLarge) => Self::new(TOO_LARGE),
            mysql::Error::IoError(e) => Self {
                lost: true,
                ..Self::new(format!("Lost connection to the MySQL server: {e}"))
            },
            mysql::Error::CodecError(e) => Self {
                lost: true,
                ..Self::new(format!("Lost connection to the MySQL server: {e}"))
            },
            mysql::Error::DriverError(
                e @ (mysql::DriverError::PacketOutOfSync | mysql::DriverError::UnexpectedPacket),
            ) => Self {
                lost: true,
                ..Self::new(format!("Lost connection to the MySQL server: {e}"))
            },
            // Their text, not `DriverError { … }` as mysql's Display writes
            // it (#222).
            mysql::Error::DriverError(e) => Self::new(e.to_string()),
            mysql::Error::TlsError(e) => Self::new(e.to_string()),
            e => Self::new(e.to_string()),
        }
    }
}

const TOO_LARGE: &str = "The statement is larger than the server's max_allowed_packet, so it \
                         wasn't sent: the session and its transaction are as they were. A larger \
                         max_allowed_packet (set global max_allowed_packet = …, by a DBA) takes \
                         effect for new connections.";

#[cfg(test)]
mod tests {
    use super::*;

    /// MySQL's errors keep their own number, without an ORA- prefix (#82).
    #[test]
    fn error_text() {
        let e = DbError::from(mysql::Error::MySqlError(mysql::MySqlError {
            state: "3D000".into(),
            message: "No database selected".into(),
            code: 1046,
        }));
        assert_eq!(e.code, Some(1046));
        assert_eq!(e.text(), "ERROR 1046 (3D000): No database selected");
    }

    /// The driver's errors read as what they say, not `DriverError { … }`;
    /// at connect, ?tls to a server without TLS says what to do (#222).
    #[test]
    fn driver_errors() {
        let e = connect_error(mysql::Error::DriverError(
            mysql::DriverError::TlsNotSupported,
        ));
        assert_eq!(
            e.message,
            "The server doesn't offer TLS, which ?tls asks for. Connect without ?tls, or turn \
             TLS on at the server."
        );
        let e = DbError::from(mysql::Error::DriverError(
            mysql::DriverError::ConnectTimeout,
        ));
        assert_eq!(e.message, "Could not connect: connection timeout");
        assert!(!e.is_connection_lost());
    }

    /// The driver refuses a statement larger than max_allowed_packet
    /// before it sends anything: the session goes on (#205). Errors of the
    /// connection itself lose it.
    #[test]
    fn packets_too_large_keep_the_session() {
        let e = DbError::from(mysql::Error::CodecError(PacketCodecError::PacketTooLarge));
        assert!(!e.is_connection_lost(), "{e}");
        assert!(e.message.contains("max_allowed_packet"), "{e}");
        assert!(!e.message.starts_with("Lost connection"), "{e}");
        let e = DbError::from(mysql::Error::DriverError(
            mysql::DriverError::PacketTooLarge,
        ));
        assert!(!e.is_connection_lost(), "{e}");
        for lost in [
            mysql::Error::CodecError(PacketCodecError::PacketsOutOfSync),
            mysql::Error::CodecError(PacketCodecError::Io(std::io::ErrorKind::BrokenPipe.into())),
            mysql::Error::DriverError(mysql::DriverError::PacketOutOfSync),
            mysql::Error::IoError(std::io::ErrorKind::ConnectionReset.into()),
        ] {
            assert!(DbError::from(lost).is_connection_lost());
        }
        // The server closes the connection on a packet it won't take.
        let e = DbError::from(mysql::Error::MySqlError(mysql::MySqlError {
            state: "08S01".into(),
            message: "Got a packet bigger than 'max_allowed_packet' bytes".into(),
            code: 1153,
        }));
        assert!(e.is_connection_lost());
    }

    #[test]
    fn targets() {
        let t = parse_target("localhost").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.database),
            ("localhost", 3306, None)
        );
        let t = parse_target("db.example.com:3307/shop").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.database.as_deref()),
            ("db.example.com", 3307, Some("shop"))
        );
        let t = parse_target("mysql://[::1]:3310/x?tls=skip-verify").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.tls),
            ("::1", 3310, Tls::SkipVerify)
        );
        assert_eq!(parse_target("h/db?tls").unwrap().tls, Tls::Verify);
        assert!(parse_target("h:port").is_err());
        assert!(parse_target("h?compress").is_err());
    }

    #[test]
    fn binds_become_positional() {
        let binds = vec![
            ("id".to_owned(), Some("7".to_owned())),
            ("n".to_owned(), None),
        ];
        let (sql, values) = positional(
            "select * from t where id = :ID or :n is null and x = ':id'",
            &binds,
        );
        assert_eq!(
            sql,
            "select * from t where id = ? or ? is null and x = ':id'"
        );
        assert_eq!(values, vec![Value::Bytes(b"7".to_vec()), Value::NULL]);
    }

    #[test]
    fn transaction_effects() {
        let cases = [
            ("select 1", TxEffect::None),
            ("/* x */ update t set a = 1", TxEffect::Changes),
            ("select * from t for update", TxEffect::Changes),
            ("call p()", TxEffect::Changes),
            ("savepoint a", TxEffect::None),
            ("rollback to savepoint a", TxEffect::None),
            ("rollback", TxEffect::Ends),
            ("create table x (a int)", TxEffect::Ends),
            ("create temporary table x (a int)", TxEffect::None),
            ("start transaction", TxEffect::Begins),
            ("commit work", TxEffect::Ends),
            // The code in `/*!…*/`, which the server runs (#219).
            ("/*!40000 ALTER TABLE `t` DISABLE KEYS */", TxEffect::Ends),
            (
                "/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER `t_bi` \
                 BEFORE INSERT ON `t` FOR EACH ROW set new.a = 1 */",
                TxEffect::Ends,
            ),
            (
                "/*!*/ /*!50000 */ insert into t values (1)",
                TxEffect::Changes,
            ),
            ("/*!40000 INSERT INTO t VALUES (1) */", TxEffect::Changes),
            ("/*!40101 SET NAMES utf8mb4 */", TxEffect::None),
        ];
        for (sql, effect) in cases {
            assert_eq!(TxEffect::of(sql), effect, "{sql}");
        }
        let mut tx = Transaction::default();
        tx.after(TxEffect::Begins);
        tx.after(TxEffect::Changes);
        assert!(tx.pending);
        tx.after(TxEffect::Ends);
        assert!(!tx.pending);
    }

    #[test]
    fn syntax_error_offsets() {
        let message = "ERROR 1064 (42000): You have an error in your SQL syntax; check the manual \
                       that corresponds to your MySQL server version for the right syntax to use \
                       near 'frm emp' at line 2";
        let sql = "select *\n  frm emp";
        assert_eq!(error_offset(sql, message), Some(11));
        let at_end = "... to use near '' at line 1";
        assert_eq!(error_offset("select * from", at_end), Some(13));
    }

    #[test]
    fn releases() {
        assert_eq!(release("9.4.0"), (9, 4));
        assert_eq!(release("10.11.6-MariaDB-0+deb12u1"), (10, 11));
    }

    /// Types as DDL names them, with UNSIGNED only on numbers (#221): MariaDB
    /// flags a TIMESTAMP, an INET6 (a string to the client), a YEAR and a
    /// BIT unsigned too.
    #[test]
    fn type_names() {
        use ColumnFlags as F;
        let col = |ty: ColumnType, flags: ColumnFlags, charset: u16| {
            type_name(
                &mysql::Column::new(ty)
                    .with_flags(flags)
                    .with_character_set(charset),
            )
        };
        let (binary, utf8) = (63, 33);
        let unsigned_binary = F::UNSIGNED_FLAG | F::BINARY_FLAG;
        for (ty, flags, charset, name) in [
            (
                ColumnType::MYSQL_TYPE_TIMESTAMP,
                unsigned_binary,
                binary,
                "TIMESTAMP",
            ),
            (
                ColumnType::MYSQL_TYPE_TIMESTAMP,
                F::BINARY_FLAG,
                binary,
                "TIMESTAMP",
            ),
            (ColumnType::MYSQL_TYPE_STRING, unsigned_binary, utf8, "CHAR"),
            (
                ColumnType::MYSQL_TYPE_YEAR,
                F::UNSIGNED_FLAG | F::ZEROFILL_FLAG,
                binary,
                "YEAR",
            ),
            (ColumnType::MYSQL_TYPE_BIT, F::UNSIGNED_FLAG, binary, "BIT"),
            (
                ColumnType::MYSQL_TYPE_LONG,
                F::UNSIGNED_FLAG | F::NUM_FLAG,
                binary,
                "INT UNSIGNED",
            ),
            (
                ColumnType::MYSQL_TYPE_NEWDECIMAL,
                F::UNSIGNED_FLAG,
                binary,
                "DECIMAL UNSIGNED",
            ),
            (
                ColumnType::MYSQL_TYPE_DOUBLE,
                F::UNSIGNED_FLAG,
                binary,
                "DOUBLE UNSIGNED",
            ),
            (
                ColumnType::MYSQL_TYPE_LONGLONG,
                F::NUM_FLAG,
                binary,
                "BIGINT",
            ),
        ] {
            assert_eq!(col(ty, flags, charset), name, "{ty:?} {flags:?}");
        }
    }
}
