use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::oracle::sql_type::OracleType;
use crate::oracle::{Connection, Connector, ResultSet, Row, StatementType};
use pythia_config::{DbKind, Privilege};
use tracing::{debug, info, warn};

use crate::dialect::Dialect;
use crate::meta::{self, MetaQuery, MetaResult};
use crate::types::{
    Bind, Cell, Client, Column, Compiled, ConnectParams, DbError, DbEvent, DbRequest, ExecSummary,
    NameContext, ServerInfo,
};

/// Interrupts the statement a worker is running (from another thread).
pub(crate) type Canceller = Arc<dyn Fn() + Send + Sync>;
/// Where a worker puts its [`Canceller`] while it is connected.
pub(crate) type CancelSlot = Arc<Mutex<Option<Canceller>>>;

/// Handle to a database session running on its own worker thread.
pub struct Session {
    requests: Sender<Envelope>,
    events: Receiver<DbEvent>,
    /// Shared with the worker so a running call can be interrupted.
    cancel: CancelSlot,
    /// A [`Session::cancel`] is on its way.
    cancelling: Arc<AtomicBool>,
    /// The handle is gone: the worker skips the statements of scripts.
    closing: Arc<AtomicBool>,
}

/// The statements of a script that a SQL window sends ahead, so its
/// session runs them one after the other without waiting for the window
/// to draw a frame in between (#291). Once one fails, unless the script
/// goes on after errors, or once it is stopped (Break's error, the window
/// closed), the session skips the rest without a word: the window counts
/// them as skipped.
#[derive(Clone, Default)]
pub struct Script(Arc<ScriptState>);

#[derive(Default)]
struct ScriptState {
    stopped: AtomicBool,
    continues: AtomicBool,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// Skips the statements not run yet.
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
    }

    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }

    /// `WHENEVER SQLERROR CONTINUE`: a statement that fails doesn't stop
    /// the script.
    pub fn continue_on_error(&self, on: bool) {
        self.0.continues.store(on, Ordering::Release);
    }

    fn failed(&self) {
        if !self.0.continues.load(Ordering::Acquire) {
            self.stop();
        }
    }
}

impl PartialEq for Script {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Script {}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Script")
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

/// A request as the worker gets it, with the script it is a statement of.
struct Envelope {
    request: DbRequest,
    script: Option<Script>,
}

/// The script statement the worker runs: its request id, and its script.
type Current = Arc<Mutex<Option<(u64, Script)>>>;

/// The worker's side of the requests: a script's statements are skipped
/// once it stopped or the session's handle is gone, and the one handed
/// out is noted, so that [`Events::send`] stops its script when it fails.
pub(crate) struct Requests {
    rx: Receiver<Envelope>,
    current: Current,
    closing: Arc<AtomicBool>,
}

impl Requests {
    pub(crate) fn recv(&self) -> Result<DbRequest, RecvError> {
        loop {
            if let Some(request) = self.take(self.rx.recv()?) {
                return Ok(request);
            }
        }
    }

    /// [`recv`](Self::recv), waiting at most `timeout`.
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<DbRequest, RecvTimeoutError> {
        let until = Instant::now() + timeout;
        loop {
            let envelope = self
                .rx
                .recv_timeout(until.saturating_duration_since(Instant::now()))?;
            if let Some(request) = self.take(envelope) {
                return Ok(request);
            }
        }
    }

    /// The request of `envelope`, none if its script stopped.
    fn take(&self, envelope: Envelope) -> Option<DbRequest> {
        let Envelope { request, script } = envelope;
        let mut current = self.current.lock().expect("script mutex");
        *current = None;
        if let Some(script) = script {
            if script.is_stopped() || self.closing.load(Ordering::Acquire) {
                debug!(?request, "skipped: its script stopped");
                return None;
            }
            if let DbRequest::Execute { id, .. } = &request {
                *current = Some((*id, script));
            }
        }
        Some(request)
    }
}

impl Session {
    /// Starts connecting in the background. `notify` is called whenever a new
    /// event is available (typically `egui::Context::request_repaint`).
    pub fn connect(params: ConnectParams, notify: impl Fn() + Send + 'static) -> Self {
        crate::install_crypto_provider();
        crate::wait_for_other_test_runs();
        let (req_tx, req_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        let cancel = Arc::new(Mutex::new(None));
        let shared = cancel.clone();
        let closing = Arc::new(AtomicBool::new(false));
        let current = Current::default();
        let req_rx = Requests {
            rx: req_rx,
            current: current.clone(),
            closing: closing.clone(),
        };
        thread::Builder::new()
            .name(format!("db-{}", params.username))
            .spawn(move || {
                let events = Events {
                    tx: ev_tx,
                    notify: Box::new(notify),
                    script: current,
                };
                match params.kind {
                    // Rather than connecting around the tunnel.
                    DbKind::Oracle if params.ssh.is_some() => events.send(DbEvent::ConnectFailed(
                        DbError::unsupported("Connecting through SSH", Dialect::ORACLE),
                    )),
                    DbKind::Oracle => Worker::run(params, req_rx, events, shared),
                    DbKind::MySql => crate::mysql_session::run(params, req_rx, events, shared),
                    DbKind::Postgres => crate::pg_session::run(params, req_rx, events, shared),
                    DbKind::SqlServer => crate::mssql_session::run(params, req_rx, events, shared),
                    // A file: nothing to tunnel to.
                    DbKind::Sqlite if params.ssh.is_some() => events.send(DbEvent::ConnectFailed(
                        DbError::unsupported("Connecting through SSH", Dialect::SQLITE),
                    )),
                    DbKind::Sqlite => crate::sqlite_session::run(params, req_rx, events, shared),
                }
            })
            .expect("spawning db worker thread");
        Self {
            requests: req_tx,
            events: ev_rx,
            cancel,
            cancelling: Arc::new(AtomicBool::new(false)),
            closing,
        }
    }

    pub fn send(&self, request: DbRequest) {
        // The worker only goes away after `Disconnect` or a failed connect;
        // requests sent after that are meaningless anyway.
        let _ = self.requests.send(Envelope {
            request,
            script: None,
        });
    }

    /// Sends a statement of `script`, which the worker skips once the
    /// script stopped.
    pub fn send_in(&self, request: DbRequest, script: &Script) {
        let _ = self.requests.send(Envelope {
            request,
            script: Some(script.clone()),
        });
    }

    pub fn try_recv(&self) -> Option<DbEvent> {
        self.events.try_recv().ok()
    }

    /// Interrupts the statement currently running on this session (OCIBreak
    /// on Oracle, `KILL QUERY` on MySQL, a cancel request on PostgreSQL,
    /// the attention signal on SQL Server, an interrupt on SQLite).
    ///
    /// Returns at once: the interrupt goes on a thread of its own, as it
    /// can wait for the network (MySQL and PostgreSQL connect to send it),
    /// and the caller is the UI (#207). One at a time: a Break while one is
    /// on its way wouldn't get through any sooner.
    pub fn cancel(&self) {
        let Some(cancel) = self.cancel.lock().expect("cancel mutex").clone() else {
            return;
        };
        if self.cancelling.swap(true, Ordering::AcqRel) {
            return;
        }
        let cancelling = self.cancelling.clone();
        let spawned = thread::Builder::new()
            .name("db-cancel".into())
            .spawn(move || {
                cancel();
                cancelling.store(false, Ordering::Release);
            });
        if let Err(e) = spawned {
            self.cancelling.store(false, Ordering::Release);
            warn!("cancel failed: {e}");
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // What a closed window's script had left doesn't run; a Commit or
        // Rollback sent before still does.
        self.closing.store(true, Ordering::Release);
        self.send(DbRequest::Disconnect);
    }
}

pub(crate) struct Events {
    tx: Sender<DbEvent>,
    notify: Box<dyn Fn() + Send>,
    /// The script statement running, whose failure stops its script.
    script: Current,
}

impl Events {
    pub(crate) fn send(&self, event: DbEvent) {
        if let DbEvent::Failed { id, .. } = &event
            && let Some((running, script)) = &*self.script.lock().expect("script mutex")
            && running == id
        {
            script.failed();
        }
        if self.tx.send(event).is_ok() {
            (self.notify)();
        }
    }
}

/// The open result set of the last query, kept for "fetch more".
struct OpenQuery {
    id: u64,
    rows: ResultSet<'static, Row>,
}

struct Worker {
    conn: Arc<Connection>,
    events: Events,
    autocommit: bool,
    open_query: Option<OpenQuery>,
    /// The current schema last reported (#320).
    schema: String,
}

impl Worker {
    fn run(params: ConnectParams, requests: Requests, events: Events, shared: CancelSlot) {
        info!(?params, "connecting");
        let conn = match connect(&params) {
            Ok(conn) => Arc::new(conn),
            Err(e) => {
                let error = DbError::from(e);
                warn!("connect failed: {error}");
                events.send(DbEvent::ConnectFailed(error));
                return;
            }
        };
        let breaker = conn.clone();
        *shared.lock().expect("cancel mutex") = Some(Arc::new(move || {
            if let Err(e) = breaker.break_execution() {
                warn!("cancel failed: {e}");
            }
        }));

        let mut worker = Worker {
            conn,
            events,
            autocommit: false,
            open_query: None,
            schema: String::new(),
        };
        match worker.on_connect(&params.init_sql) {
            Ok((server, failed)) => {
                worker.schema = server.current_schema.clone();
                worker.events.send(DbEvent::Connected(server));
                for notice in failed {
                    worker.events.send(DbEvent::Notice(notice));
                }
            }
            Err(e) => worker.events.send(DbEvent::ConnectFailed(e)),
        }

        while let Ok(request) = requests.recv() {
            debug!(?request, "db request");
            match request {
                DbRequest::Execute {
                    id,
                    sql,
                    max_rows,
                    binds,
                } => worker.execute(id, &sql, max_rows, &binds),
                // Values are whole; Execute to file streams through a task.
                DbRequest::ExecuteWhole { id, sql, max_rows } => {
                    worker.execute(id, &sql, max_rows, &[])
                }
                DbRequest::Meta { id, query } => {
                    let started = Instant::now();
                    let result = meta::run(&worker.conn, &query);
                    debug!(
                        id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        ok = result.is_ok(),
                        ?query,
                        "metadata query done"
                    );
                    worker.events.send(DbEvent::Meta { id, result });
                }
                DbRequest::FetchMore { id, max_rows } => worker.fetch_more(id, max_rows),
                DbRequest::Commit => worker.end_transaction(true),
                DbRequest::Rollback => worker.end_transaction(false),
                DbRequest::SetAutocommit(on) => worker.autocommit = on,
                DbRequest::Task { id, task } => {
                    let started = Instant::now();
                    let result = task(&worker.conn);
                    debug!(
                        id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        ok = result.is_ok(),
                        "task done"
                    );
                    // Tasks (data edits, debug runs, …) can open a
                    // transaction; its state arrives before the result.
                    worker.report_transaction();
                    worker.events.send(DbEvent::TaskDone { id, result });
                }
                DbRequest::Run { id, task } => {
                    let started = Instant::now();
                    let result = task(&mut OracleClient(&worker.conn));
                    debug!(
                        id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        ok = result.is_ok(),
                        "task done"
                    );
                    worker.events.send(DbEvent::TaskDone { id, result });
                    worker.report_transaction();
                }
                DbRequest::Disconnect => break,
            }
        }

        worker.open_query = None;
        *shared.lock().expect("cancel mutex") = None;
        if let Err(e) = worker.conn.close() {
            warn!("closing connection: {e}");
        }
        info!("disconnected");
        worker.events.send(DbEvent::Disconnected);
    }

    /// Sets the session up and runs the login statements, which may set
    /// the current schema (#103): it is read after them. Their failures
    /// come back as notices.
    fn on_connect(&self, init_sql: &[String]) -> Result<(ServerInfo, Vec<String>), DbError> {
        let (version, banner) = self.conn.server_version()?;
        self.conn.set_module("pythia")?;
        self.conn
            .execute("begin dbms_output.enable(null); end;", &[])?;
        let mut failed = Vec::new();
        for sql in init_sql {
            if let Err(e) = self.conn.execute(sql, &[]) {
                let e = DbError::from(e);
                failed.push(format!(
                    "Login script: {e} ({})",
                    sql.lines().next().unwrap_or_default()
                ));
            }
        }
        let schema = current_schema(&self.conn)?;
        let server = ServerInfo {
            dialect: Dialect::ORACLE,
            version: version.to_string(),
            release: (version.major(), version.minor()),
            banner,
            context: NameContext::schema(&schema),
            current_schema: schema,
        };
        Ok((server, failed))
    }

    fn execute(&mut self, id: u64, sql: &str, max_rows: usize, binds: &[(String, Bind)]) {
        // The last query's rows left can't be fetched after it.
        if let Some(open) = self.open_query.take() {
            self.events.send(DbEvent::QueryClosed { id: open.id });
        }
        let started = Instant::now();
        let done = match self.execute_inner(id, sql, max_rows, binds, started) {
            Ok(done) => done,
            Err(error) => Some(DbEvent::Failed {
                id,
                error,
                elapsed: started.elapsed(),
            }),
        };
        // Output and the transaction state come first: the event that ends
        // the statement finds them up to date.
        self.after_statement();
        if let Some(done) = done {
            self.events.send(done);
        }
    }

    /// Runs a statement; returns the event that ends it (rows of a query,
    /// or what was executed).
    fn execute_inner(
        &mut self,
        id: u64,
        sql: &str,
        max_rows: usize,
        binds: &[(String, Bind)],
        started: Instant,
    ) -> Result<Option<DbEvent>, DbError> {
        let values: Vec<Bound<'_>> = binds.iter().map(|(_, value)| Bound(value)).collect();
        let named: Vec<(&str, &dyn crate::oracle::sql_type::ToSql)> = binds
            .iter()
            .zip(&values)
            .map(|((name, _), value)| (name.as_str(), value as &dyn crate::oracle::sql_type::ToSql))
            .collect();
        let mut stmt = self.conn.statement(sql).build()?;
        if stmt.is_query() {
            drop(stmt);
            let rows = self.conn.query_named(sql, &named)?;
            let columns = rows
                .column_info()
                .iter()
                .map(|c| column(c.name(), c.oracle_type()))
                .collect();
            self.events.send(DbEvent::QueryStarted { id, columns });
            self.open_query = Some(OpenQuery { id, rows });
            return Ok(self.fetch(max_rows, started));
        }

        stmt.execute_named(&named)?;
        let kind = stmt.statement_type();
        let rows_affected = stmt.is_dml().then(|| stmt.row_count()).transpose()?;
        if self.autocommit && stmt.is_dml() {
            self.conn.commit()?;
        }
        let compiled = match kind {
            StatementType::Create => self.compiled(sql)?,
            _ => None,
        };
        let summary = ExecSummary {
            kind: statement_kind_label(&kind, sql),
            rows_affected,
            compiled,
        };
        Ok(Some(DbEvent::Executed {
            id,
            summary,
            elapsed: started.elapsed(),
        }))
    }

    /// After `CREATE` of a stored unit, reads its compile errors.
    fn compiled(&self, sql: &str) -> Result<Option<Compiled>, DbError> {
        let Some(header) = pythia_plsql::unit_header(sql) else {
            return Ok(None);
        };
        let owner = match header.owner {
            Some(owner) => owner,
            None => current_schema(&self.conn)?,
        };
        let errors = meta::errors(&self.conn, &owner, &header.name, &header.object_type)?;
        Ok(Some(Compiled {
            owner,
            name: header.name,
            object_type: header.object_type,
            errors,
        }))
    }

    fn fetch_more(&mut self, id: u64, max_rows: usize) {
        if self.open_query.as_ref().is_none_or(|q| q.id != id) {
            // Closed by a statement after it in between.
            self.events.send(DbEvent::QueryClosed { id });
        } else if let Some(rows) = self.fetch(max_rows, Instant::now()) {
            self.events.send(rows);
        }
    }

    /// Fetches up to `max_rows` from the open query: the rows, or the
    /// error.
    fn fetch(&mut self, max_rows: usize, started: Instant) -> Option<DbEvent> {
        let query = self.open_query.as_mut()?;
        let id = query.id;
        let mut rows = Vec::with_capacity(max_rows.min(10_000));
        let mut more = true;
        while rows.len() < max_rows {
            match query.rows.next() {
                Some(Ok(row)) => rows.push(row.sql_values().iter().map(cell).collect()),
                Some(Err(e)) => {
                    self.open_query = None;
                    return Some(DbEvent::Failed {
                        id,
                        error: e.into(),
                        elapsed: started.elapsed(),
                    });
                }
                None => {
                    more = false;
                    break;
                }
            }
        }
        if !more {
            self.open_query = None;
        }
        Some(DbEvent::Rows {
            id,
            rows,
            more,
            elapsed: started.elapsed(),
        })
    }

    fn end_transaction(&mut self, commit: bool) {
        let result = if commit {
            self.conn.commit()
        } else {
            self.conn.rollback()
        };
        match result {
            Ok(()) => self.events.send(DbEvent::Notice(
                if commit { "Committed" } else { "Rolled back" }.into(),
            )),
            Err(e) => self.events.send(DbEvent::Error(e.into())),
        }
        self.after_statement();
    }

    /// Collects DBMS_OUTPUT and refreshes the transaction state.
    fn after_statement(&mut self) {
        match self.read_output() {
            Ok(lines) if !lines.is_empty() => self.events.send(DbEvent::Output(lines)),
            Ok(_) => {}
            Err(e) => warn!("reading dbms_output: {e}"),
        }
        self.report_transaction();
    }

    /// Sends whether the session has an open transaction, and its current
    /// schema when that changed (#320), read with it.
    fn report_transaction(&mut self) {
        match self.conn.query_row_as::<(Option<String>, String)>(
            "select dbms_transaction.local_transaction_id, \
                    sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
            &[],
        ) {
            Ok((tx, schema)) => {
                if self.schema != schema {
                    self.events
                        .send(DbEvent::Context(NameContext::schema(&schema)));
                    self.schema = schema;
                }
                self.events.send(DbEvent::Transaction {
                    pending: tx.is_some(),
                });
            }
            Err(e) => warn!("reading transaction state: {e}"),
        }
    }

    fn read_output(&self) -> Result<Vec<String>, DbError> {
        let mut stmt = self
            .conn
            .statement("begin dbms_output.get_line(:1, :2); end;")
            .build()?;
        let mut lines = Vec::new();
        loop {
            stmt.execute(&[&OracleType::Varchar2(32767), &OracleType::Number(0, 0)])?;
            let status: i32 = stmt.bind_value(2)?;
            if status != 0 {
                return Ok(lines);
            }
            lines.push(stmt.bind_value::<_, Option<String>>(1)?.unwrap_or_default());
        }
    }
}

/// A bind's value as the driver binds it: text as a string; bytes as RAW
/// and text as VARCHAR2, whole (#293). The thin driver binds a value over
/// 32767 bytes as LONG RAW or LONG itself, which also go into a LOB.
struct Bound<'a>(&'a Bind);

#[cfg(not(feature = "thick"))]
impl crate::oracle::sql_type::ToSql for Bound<'_> {
    fn to_bind(&self) -> crate::oracle::Bind {
        match self.0 {
            Bind::Text(value) => value.to_bind(),
            Bind::Bytes(bytes) => (**bytes).to_bind(),
            Bind::LongText(text) => (**text).to_bind(),
        }
    }
}

/// The Instant Client binds RAW and VARCHAR2 of up to 32767 bytes: past
/// that, as LONG RAW and LONG.
#[cfg(feature = "thick")]
const LONGEST_INLINE: usize = 32767;

#[cfg(feature = "thick")]
impl ::oracle::sql_type::ToSql for Bound<'_> {
    fn oratype(&self, conn: &::oracle::Connection) -> ::oracle::Result<OracleType> {
        match self.0 {
            Bind::Text(value) => value.oratype(conn),
            Bind::Bytes(bytes) if bytes.len() > LONGEST_INLINE => Ok(OracleType::LongRaw),
            Bind::Bytes(bytes) => (&**bytes).oratype(conn),
            Bind::LongText(text) if text.len() > LONGEST_INLINE => Ok(OracleType::Long),
            Bind::LongText(text) => (&**text).oratype(conn),
        }
    }

    fn to_sql(&self, val: &mut ::oracle::SqlValue) -> ::oracle::Result<()> {
        match self.0 {
            Bind::Text(value) => value.to_sql(val),
            Bind::Bytes(bytes) => (&**bytes).to_sql(val),
            Bind::LongText(text) => (&**text).to_sql(val),
        }
    }
}

/// An Oracle connection as a [`Client`].
struct OracleClient<'a>(&'a Connection);

impl Client for OracleClient<'_> {
    fn dialect(&self) -> Dialect {
        Dialect::ORACLE
    }

    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Cell>>, DbError> {
        let params: Vec<&dyn crate::oracle::sql_type::ToSql> = params
            .iter()
            .map(|p| p as &dyn crate::oracle::sql_type::ToSql)
            .collect();
        let mut out = Vec::new();
        for row in self.0.query(sql, &params)? {
            out.push(row?.sql_values().iter().map(cell).collect());
        }
        Ok(out)
    }

    fn meta(&mut self, query: &MetaQuery) -> Result<MetaResult, DbError> {
        meta::run(self.0, query)
    }

    fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        Ok(self.0.execute(sql, &[]).map(|_| ())?)
    }
}

/// An Oracle connection of its own (not behind a [`Session`]), e.g. for a
/// task that works in parallel. Runs the login statements; their errors
/// don't fail it.
pub fn open_connection(params: &ConnectParams) -> Result<Connection, DbError> {
    if params.ssh.is_some() {
        return Err(DbError::unsupported(
            "Connecting through SSH",
            Dialect::ORACLE,
        ));
    }
    let conn = connect(params)?;
    conn.set_module("pythia")?;
    for sql in &params.init_sql {
        if let Err(e) = conn.execute(sql, &[]) {
            warn!("login script on a worker session: {e}");
        }
    }
    Ok(conn)
}

fn connect(params: &ConnectParams) -> crate::oracle::Result<Connection> {
    let mut connector = Connector::new(&params.username, &params.password, &params.connect_string);
    let privilege = match params.privilege {
        Privilege::Normal => None,
        Privilege::Sysdba => Some(crate::oracle::Privilege::Sysdba),
        Privilege::Sysoper => Some(crate::oracle::Privilege::Sysoper),
        Privilege::Sysbackup => Some(crate::oracle::Privilege::Sysbackup),
    };
    if let Some(privilege) = privilege {
        connector.privilege(privilege);
    }
    // An empty username with a connect string means external auth (`/@alias`).
    if params.username.is_empty() {
        connector.external_auth(true);
    }
    connector.connect()
}

/// Runs a query and hands its rows to `sink` in batches, without keeping
/// them (e.g. to stream a large result into a file). The first call has no
/// rows, so the columns are known even for an empty result. Returns the row
/// count.
pub fn stream_query(
    conn: &Connection,
    sql: &str,
    batch: usize,
    mut sink: impl FnMut(&[Column], &[Vec<Cell>]) -> Result<(), DbError>,
) -> Result<u64, DbError> {
    let mut stmt = conn.statement(sql).fetch_array_size(batch as u32).build()?;
    let rows = stmt.query(&[])?;
    let columns: Vec<Column> = rows
        .column_info()
        .iter()
        .map(|c| column(c.name(), c.oracle_type()))
        .collect();
    sink(&columns, &[])?;
    let mut buffer = Vec::with_capacity(batch);
    let mut count = 0u64;
    for row in rows {
        let row = row?;
        buffer.push(row.sql_values().iter().map(cell).collect());
        if buffer.len() == batch {
            count += buffer.len() as u64;
            sink(&columns, &buffer)?;
            buffer.clear();
        }
    }
    count += buffer.len() as u64;
    if !buffer.is_empty() {
        sink(&columns, &buffer)?;
    }
    Ok(count)
}

fn column(name: &str, ty: &OracleType) -> Column {
    let numeric = matches!(
        ty,
        OracleType::Number(..)
            | OracleType::Float(_)
            | OracleType::BinaryFloat
            | OracleType::BinaryDouble
            | OracleType::Int64
            | OracleType::UInt64
    );
    Column {
        name: name.to_owned(),
        type_name: ty.to_string(),
        numeric,
    }
}

fn cell(value: &crate::oracle::SqlValue) -> Cell {
    match value.is_null() {
        Ok(true) => None,
        Ok(false) => Some(text(value)),
        Err(e) => Some(format!("<{e}>")),
    }
}

/// A value as text. The Instant Client's BINARY_DOUBLE and BINARY_FLOAT
/// as the pure-Rust driver writes them, with an exponent where it is long
/// (#304): the `oracle` crate writes 1e300 as a 1 and 300 zeros.
fn text(value: &crate::oracle::SqlValue) -> String {
    #[cfg(feature = "thick")]
    match value.oracle_type() {
        Ok(OracleType::BinaryDouble) => {
            if let Ok(v) = value.get::<f64>() {
                return crate::float_text(v);
            }
        }
        Ok(OracleType::BinaryFloat) => {
            if let Ok(v) = value.get::<f32>() {
                return crate::float_text(v);
            }
        }
        _ => {}
    }
    value.get::<String>().unwrap_or_else(|_| value.to_string())
}

fn statement_kind_label(kind: &StatementType, sql: &str) -> String {
    match kind {
        StatementType::Begin | StatementType::Declare => "PL/SQL block".into(),
        // Fall back to the leading keyword(s), e.g. `CREATE`, `GRANT`, `TRUNCATE`.
        _ => sql
            .split_whitespace()
            .next()
            .unwrap_or("Statement")
            .to_uppercase(),
    }
}

/// The session's current schema. `Connection::current_schema` only reports a
/// value after it was set explicitly, so ask the database.
fn current_schema(conn: &Connection) -> crate::oracle::Result<String> {
    conn.query_row_as(
        "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
        &[],
    )
}
