//! SQLite sessions: the requests and events of an Oracle session, served by
//! SQLite itself (compiled in through `rusqlite`, nothing to install).
//!
//! A session is a connection to the database file, so every window has its
//! own as on a server. A query's rows are stepped while the worker waits for
//! `FetchMore`s of that query; any other request finishes the statement
//! first.
//!
//! Transactions behave like Oracle's: changes wait for Commit unless the
//! session autocommits, as a change outside a transaction begins one. SQLite
//! says itself whether a transaction is open. DDL outside a transaction is
//! committed at once; inside one it waits for Commit too (SQLite's DDL is
//! transactional). While a session has uncommitted changes, another one that
//! writes waits up to [`BUSY_TIMEOUT`] for the lock, then fails with
//! "database is locked".

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags, Rows, Statement, params_from_iter};
use tracing::{debug, info, warn};

use crate::dialect::Dialect;
use crate::meta::{MetaQuery, MetaResult};
use crate::session::{CancelSlot, Canceller, Events};
use crate::sql_text::{hex, release, replace_binds, statement_kind};
use crate::types::{
    Cell, Client, Column, ConnectParams, DbError, DbEvent, DbRequest, ExecSummary, ServerInfo,
};

/// How long a statement waits for another session's lock on the file.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// `:memory:` is one database for all sessions of the app (each would
/// otherwise get an empty one of its own), kept while one of them is open.
const SHARED_MEMORY: &str = "file:pythia-memory?mode=memory&cache=shared";

pub(crate) fn run(
    params: ConnectParams,
    requests: Receiver<DbRequest>,
    events: Events,
    shared: CancelSlot,
) {
    info!(?params, "connecting");
    let conn = match open(&params.connect_string) {
        Ok(conn) => conn,
        Err(error) => {
            warn!("connect failed: {error}");
            events.send(DbEvent::ConnectFailed(error));
            return;
        }
    };
    let interrupt = conn.get_interrupt_handle();
    let cancel: Canceller = Arc::new(move || interrupt.interrupt());
    *shared.lock().expect("cancel mutex") = Some(cancel);

    let mut worker = Worker {
        conn,
        events,
        autocommit: false,
    };
    match worker.on_connect(&params.init_sql) {
        Ok((server, failed)) => {
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
            } => next = worker.execute(id, &sql, max_rows, &binds, &requests),
            // Its result was closed by another request in between.
            DbRequest::FetchMore { id, .. } => worker.events.send(DbEvent::Rows {
                id,
                rows: Vec::new(),
                more: false,
                elapsed: Duration::ZERO,
            }),
            DbRequest::Meta { id, query } => {
                let started = Instant::now();
                let result = crate::sqlite_meta::run(&worker.conn, &query);
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
                result: Err(DbError::unsupported("This tool", Dialect::SQLITE)),
            }),
            DbRequest::Run { id, task } => {
                let started = Instant::now();
                let result = task(&mut SqliteClient(&worker.conn));
                debug!(
                    id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    ok = result.is_ok(),
                    "task done"
                );
                worker.events.send(DbEvent::TaskDone { id, result });
                // A task may have changed something (maintenance commits).
                worker.report_transaction();
            }
            DbRequest::Disconnect => break,
        }
    }

    *shared.lock().expect("cancel mutex") = None;
    // Closing the connection rolls back what wasn't committed.
    drop(worker.conn);
    info!("disconnected");
    worker.events.send(DbEvent::Disconnected);
}

/// Opens the database `path[?ro][&create]`: an existing file (`~` for the
/// home directory), `:memory:` or a `file:` URI as SQLite takes it. A
/// missing file is only made with `create`, so a typo doesn't leave an
/// empty database behind.
pub(crate) fn open(connect_string: &str) -> Result<Connection, DbError> {
    let target = Target::parse(connect_string)?;
    let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    flags |= if target.read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let location = match &target.file {
        File::Memory => SHARED_MEMORY.to_owned(),
        File::Uri(uri) => {
            if !target.read_only {
                // The URI's `mode=` decides; without one SQLite creates.
                flags |= OpenFlags::SQLITE_OPEN_CREATE;
            }
            uri.clone()
        }
        File::Path(path) => {
            if target.create && !target.read_only {
                flags |= OpenFlags::SQLITE_OPEN_CREATE;
            } else if !path.is_file() {
                return Err(DbError::new(if path.exists() {
                    format!("{} is a directory, not a database file", path.display())
                } else {
                    format!(
                        "There is no database file {} (add ?create to make a new one)",
                        path.display()
                    )
                }));
            }
            // A path, not a URI: `file:` takes `?` and `#` as its own.
            let text = path.to_string_lossy();
            if text.contains(['?', '#']) {
                flags.remove(OpenFlags::SQLITE_OPEN_URI);
            }
            text.into_owned()
        }
    };
    let conn = Connection::open_with_flags(&location, flags).map_err(DbError::from)?;
    // Writing waits for another session's lock instead of failing at once.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // Not a database at all: SQLite only notices on the first read.
    conn.query_row("select count(*) from sqlite_schema", [], |_| Ok(()))
        .map_err(|e| {
            let mut error = DbError::from(e);
            if let File::Path(path) = &target.file {
                error.message = format!("{}: {}", path.display(), error.message);
            }
            error
        })?;
    Ok(conn)
}

#[derive(Debug, PartialEq, Eq)]
enum File {
    Path(PathBuf),
    Memory,
    Uri(String),
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    file: File,
    read_only: bool,
    create: bool,
}

impl Target {
    fn parse(text: &str) -> Result<Target, DbError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(DbError::new("Which database file? Give its path"));
        }
        if text.starts_with("file:") {
            return Ok(Target {
                read_only: text.contains("mode=ro"),
                file: File::Uri(text.to_owned()),
                create: false,
            });
        }
        let text = text.strip_prefix("sqlite://").unwrap_or(text);
        // Options after the last `?`, if they all are ours: a file name may
        // have a `?` of its own.
        let (path, options) = match text.rsplit_once('?') {
            Some((path, options)) if options.split('&').all(|o| Self::option(o).is_some()) => {
                (path, options)
            }
            _ => (text, ""),
        };
        let mut target = Target {
            file: File::Memory,
            read_only: false,
            create: false,
        };
        for option in options.split('&').filter(|o| !o.is_empty()) {
            match Self::option(option) {
                Some(Opt::ReadOnly) => target.read_only = true,
                Some(Opt::Create) => target.create = true,
                None => unreachable!("checked above"),
            }
        }
        target.file = match path.trim() {
            ":memory:" => File::Memory,
            path => File::Path(expand_home(path)),
        };
        Ok(target)
    }

    fn option(option: &str) -> Option<Opt> {
        match option.trim().to_ascii_lowercase().as_str() {
            "ro" | "readonly" | "read-only" | "mode=ro" => Some(Opt::ReadOnly),
            "create" | "mode=rwc" => Some(Opt::Create),
            _ => None,
        }
    }
}

enum Opt {
    ReadOnly,
    Create,
}

/// `~/x.db` → the home directory's `x.db`.
fn expand_home(path: &str) -> PathBuf {
    let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"));
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match (rest, home) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

struct Worker {
    conn: Connection,
    events: Events,
    autocommit: bool,
}

impl Worker {
    /// Sets the session up and runs the login statements; their failures
    /// come back as notices.
    fn on_connect(&mut self, init_sql: &[String]) -> Result<(ServerInfo, Vec<String>), DbError> {
        // As most tools do: SQLite checks foreign keys only when asked to, per
        // connection. A login script can turn it off again.
        self.conn.execute_batch("pragma foreign_keys = on")?;
        let mut failed = Vec::new();
        for sql in init_sql {
            if let Err(e) = self.conn.execute_batch(sql) {
                let e = DbError::from(e);
                failed.push(format!(
                    "Login script: {e} ({})",
                    sql.lines().next().unwrap_or_default()
                ));
            }
        }
        let version: String = self
            .conn
            .query_row("select sqlite_version()", [], |row| row.get(0))?;
        let file = self
            .conn
            .path()
            .filter(|p| !p.is_empty())
            .map(str::to_owned);
        let server = ServerInfo {
            dialect: Dialect::SQLITE,
            release: release(&version),
            banner: match file {
                Some(file) => format!("SQLite {version} ({file})"),
                None => format!("SQLite {version} (in memory)"),
            },
            version,
            current_schema: "main".into(),
        };
        Ok((server, failed))
    }

    /// Whether a transaction is open, as SQLite says.
    fn pending(&self) -> bool {
        !self.conn.is_autocommit()
    }

    /// Runs a statement. A query's result stays open for `FetchMore`s;
    /// returns the request that closed it, if any.
    fn execute(
        &mut self,
        id: u64,
        sql: &str,
        max_rows: usize,
        binds: &[(String, Option<String>)],
        requests: &Receiver<DbRequest>,
    ) -> Option<DbRequest> {
        let started = Instant::now();
        let mut next = None;
        let (sql, values) = positional(sql, binds);
        // Changes wait for Commit: they get a transaction of their own.
        let opened = !self.autocommit && !self.pending() && changes(&sql);
        if opened && let Err(e) = self.conn.execute_batch("begin") {
            self.events.send(DbEvent::Failed {
                id,
                error: e.into(),
                elapsed: started.elapsed(),
            });
            return None;
        }
        let fetch = Fetch {
            id,
            max_rows,
            started,
            events: &self.events,
            requests,
        };
        match run_statement(&self.conn, &sql, values, fetch, &mut next) {
            Ok(Outcome::Query { failed }) => {
                if failed && opened && self.pending() {
                    let _ = self.conn.execute_batch("rollback");
                }
            }
            Ok(Outcome::Done { affected }) => {
                let kind = statement_kind(&sql);
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
            }
            Err(e) => {
                let mut error = DbError::from(e);
                if error.offset.is_none() {
                    error.offset = name_offset(&sql, &error.message);
                }
                // An offset in the statement as sent, which binds changed.
                if !binds.is_empty() {
                    error.offset = None;
                }
                // Nothing to keep from a failed change of its own.
                if opened && self.pending() {
                    let _ = self.conn.execute_batch("rollback");
                }
                self.events.send(DbEvent::Failed {
                    id,
                    error,
                    elapsed: started.elapsed(),
                });
            }
        }
        self.report_transaction();
        next
    }

    fn end_transaction(&mut self, commit: bool) {
        // Without a transaction there is nothing to end; SQLite would say
        // "no transaction is active".
        let result = if self.pending() {
            self.conn
                .execute_batch(if commit { "commit" } else { "rollback" })
        } else {
            Ok(())
        };
        match result {
            Ok(()) => self.events.send(DbEvent::Notice(
                if commit { "Committed" } else { "Rolled back" }.into(),
            )),
            Err(e) => self.events.send(DbEvent::Error(e.into())),
        }
        self.report_transaction();
    }

    fn report_transaction(&self) {
        self.events.send(DbEvent::Transaction {
            pending: self.pending(),
        });
    }
}

/// How a statement ended: a query's rows were reported as they were
/// stepped, anything else is summarized.
enum Outcome {
    Query { failed: bool },
    Done { affected: u64 },
}

fn run_statement(
    conn: &Connection,
    sql: &str,
    values: Vec<Value>,
    fetch: Fetch<'_>,
    next: &mut Option<DbRequest>,
) -> Result<Outcome, rusqlite::Error> {
    let mut statement = conn.prepare(sql)?;
    if statement.column_count() == 0 {
        statement.execute(params_from_iter(values))?;
        return Ok(Outcome::Done {
            affected: conn.changes(),
        });
    }
    let declared: Vec<Option<String>> = statement
        .columns()
        .map(|c| c.decl_type().map(str::to_owned))
        .collect();
    let names: Vec<String> = statement.column_names().map(str::to_owned).collect();
    let rows = statement.query(params_from_iter(values))?;
    Ok(fetch.stream(rows, &names, &declared, next))
}

/// Reads a query's rows for [`run_statement`].
struct Fetch<'a> {
    id: u64,
    max_rows: usize,
    started: Instant,
    events: &'a Events,
    requests: &'a Receiver<DbRequest>,
}

impl Fetch<'_> {
    fn stream(
        self,
        mut rows: Rows<'_>,
        names: &[String],
        declared: &[Option<String>],
        next: &mut Option<DbRequest>,
    ) -> Outcome {
        let id = self.id;
        let mut max_rows = self.max_rows;
        let mut started = self.started;
        let mut columns: Option<Vec<Column>> = None;
        loop {
            let mut batch = Vec::with_capacity(max_rows.min(10_000));
            // The value types of the first rows, for columns without a
            // declared type.
            let mut seen = vec![Seen::default(); if columns.is_none() { names.len() } else { 0 }];
            let mut more = true;
            let mut failed = None;
            while batch.len() < max_rows {
                match rows.next() {
                    Ok(Some(row)) => {
                        let mut cells = Vec::with_capacity(names.len());
                        for i in 0..names.len() {
                            let value = row.get_ref(i).unwrap_or(ValueRef::Null);
                            if let Some(seen) = seen.get_mut(i) {
                                seen.note(value);
                            }
                            cells.push(cell(value));
                        }
                        batch.push(cells);
                    }
                    Ok(None) => {
                        more = false;
                        break;
                    }
                    Err(e) => {
                        failed = Some(DbError::from(e));
                        more = false;
                        break;
                    }
                }
            }
            if columns.is_none() {
                let described: Vec<Column> = names
                    .iter()
                    .zip(declared)
                    .zip(&seen)
                    .map(|((name, declared), seen)| column(name, declared.as_deref(), seen))
                    .collect();
                self.events.send(DbEvent::QueryStarted {
                    id,
                    columns: described.clone(),
                });
                columns = Some(described);
            }
            if let Some(error) = failed {
                self.events.send(DbEvent::Failed {
                    id,
                    error,
                    elapsed: started.elapsed(),
                });
                return Outcome::Query { failed: true };
            }
            self.events.send(DbEvent::Rows {
                id,
                rows: batch,
                more,
                elapsed: started.elapsed(),
            });
            if !more {
                return Outcome::Query { failed: false };
            }
            match self.requests.recv() {
                Ok(DbRequest::FetchMore {
                    id: wanted,
                    max_rows: n,
                }) if wanted == id => {
                    max_rows = n;
                    started = Instant::now();
                }
                other => {
                    // Dropping the rows resets the statement: nothing more is
                    // read.
                    *next = Some(other.unwrap_or(DbRequest::Disconnect));
                    return Outcome::Query { failed: false };
                }
            }
        }
    }
}

/// What kinds of values a column of a result had.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    integer: bool,
    real: bool,
    text: bool,
    blob: bool,
}

impl Seen {
    fn note(&mut self, value: ValueRef<'_>) {
        match value {
            ValueRef::Null => {}
            ValueRef::Integer(_) => self.integer = true,
            ValueRef::Real(_) => self.real = true,
            ValueRef::Text(_) => self.text = true,
            ValueRef::Blob(_) => self.blob = true,
        }
    }

    /// Only numbers (and NULLs), at least one.
    fn numbers(self) -> bool {
        (self.integer || self.real) && !self.text && !self.blob
    }

    /// The storage class most of them had, for a column without a declared
    /// type.
    fn type_name(self) -> &'static str {
        match self {
            Seen { text: true, .. } => "TEXT",
            Seen { blob: true, .. } => "BLOB",
            Seen { real: true, .. } => "REAL",
            Seen { integer: true, .. } => "INTEGER",
            _ => "",
        }
    }
}

/// A result column: its declared type, or the values' when it has none (an
/// expression). Numbers are right aligned: a column with INTEGER or REAL
/// affinity, or one whose values were all numbers.
fn column(name: &str, declared: Option<&str>, seen: &Seen) -> Column {
    let type_name = match declared {
        Some(declared) => declared.to_ascii_uppercase(),
        None => seen.type_name().to_owned(),
    };
    let numeric = match affinity(&type_name) {
        Affinity::Integer | Affinity::Real => !seen.text && !seen.blob,
        Affinity::Text => false,
        Affinity::Numeric | Affinity::Blob => seen.numbers(),
    };
    Column {
        name: name.to_owned(),
        type_name,
        numeric,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// The type affinity of a declared type, by SQLite's rules (section 3.1 of
/// "Datatypes In SQLite").
pub(crate) fn affinity(declared: &str) -> Affinity {
    let upper = declared.to_ascii_uppercase();
    if upper.contains("INT") {
        Affinity::Integer
    } else if ["CHAR", "CLOB", "TEXT"].iter().any(|t| upper.contains(t)) {
        Affinity::Text
    } else if upper.contains("BLOB") || upper.trim().is_empty() {
        Affinity::Blob
    } else if ["REAL", "FLOA", "DOUB"].iter().any(|t| upper.contains(t)) {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// A value as text: numbers as SQLite writes them (`1.0` for a real),
/// blobs in hex.
pub(crate) fn cell(value: ValueRef<'_>) -> Cell {
    Some(match value {
        ValueRef::Null => return None,
        ValueRef::Integer(i) => i.to_string(),
        // `{:?}`: shortest round trip, keeps `.0`, exponent for large and
        // tiny values.
        ValueRef::Real(f) => format!("{f:?}"),
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        ValueRef::Blob(bytes) => hex(bytes),
    })
}

/// Replaces `:name` binds by `?` with their values in order.
fn positional(sql: &str, binds: &[(String, Option<String>)]) -> (String, Vec<Value>) {
    if binds.is_empty() {
        return (sql.to_owned(), Vec::new());
    }
    let mut values = Vec::new();
    let sql = replace_binds(sql, binds, |_, value| {
        values.push(match value {
            Some(v) => bind_value(v),
            None => Value::Null,
        });
        "?".into()
    });
    (sql, values)
}

/// A bind's text as SQLite should compare it. A number written the way
/// SQLite writes it is bound as one: a column without a type (no affinity)
/// would find nothing for the text `7`. Anything else stays text, which a
/// column of numeric affinity converts itself, and `007` keeps its zeros.
pub(crate) fn bind_value(text: &str) -> Value {
    if let Ok(i) = text.parse::<i64>()
        && i.to_string() == text
    {
        return Value::Integer(i);
    }
    if let Ok(f) = text.parse::<f64>()
        && f.is_finite()
        && format!("{f:?}") == text
    {
        return Value::Real(f);
    }
    Value::Text(text.to_owned())
}

/// Where the name an error is about first appears in the statement, when
/// SQLite doesn't say (`no such table: emp`).
fn name_offset(sql: &str, message: &str) -> Option<u32> {
    let name = ["no such table: ", "no such view: ", "no such function: "]
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix))?;
    let name = name.rsplit('.').next()?.trim();
    let token = pythia_plsql::tokenize(sql).into_iter().find(|t| {
        let text = &sql[t.span.clone()];
        (t.kind.is_word() || t.kind == pythia_plsql::TokenKind::QuotedIdent)
            && text
                .trim_matches(['"', '`', '[', ']'])
                .eq_ignore_ascii_case(name)
    })?;
    u32::try_from(token.span.start).ok()
}

fn is_dml(kind: &str) -> bool {
    matches!(kind, "INSERT" | "UPDATE" | "DELETE" | "REPLACE")
}

/// Whether the statement changes rows, and so waits for Commit.
fn changes(sql: &str) -> bool {
    let words = crate::sql_text::leading_words(sql);
    match words.first().map(String::as_str).unwrap_or_default() {
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" => true,
        "WITH" => crate::sql_text::has_word(sql, &["INSERT", "UPDATE", "DELETE", "REPLACE"]),
        _ => false,
    }
}

/// A SQLite connection as a [`Client`].
pub(crate) struct SqliteClient<'a>(pub(crate) &'a Connection);

impl Client for SqliteClient<'_> {
    fn dialect(&self) -> Dialect {
        Dialect::SQLITE
    }

    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Cell>>, DbError> {
        let mut statement = self.0.prepare(sql)?;
        let values: Vec<Value> = params.iter().map(|p| bind_value(p)).collect();
        Ok(rows_of(&mut statement, values)?)
    }

    fn meta(&mut self, query: &MetaQuery) -> Result<MetaResult, DbError> {
        crate::sqlite_meta::run(self.0, query)
    }

    fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        Ok(self.0.execute_batch(sql)?)
    }
}

/// All rows of a statement as text; `params` are its `?`s in order, as
/// text.
pub(crate) fn all_rows(
    statement: &mut Statement<'_>,
    params: &[&str],
) -> Result<Vec<Vec<Cell>>, rusqlite::Error> {
    rows_of(
        statement,
        params.iter().map(|p| Value::Text((*p).to_owned())),
    )
}

fn rows_of(
    statement: &mut Statement<'_>,
    params: impl IntoIterator<Item = Value>,
) -> Result<Vec<Vec<Cell>>, rusqlite::Error> {
    let count = statement.column_count();
    let mut rows = statement.query(params_from_iter(params))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(
            (0..count)
                .map(|i| cell(row.get_ref(i).unwrap_or(ValueRef::Null)))
                .collect(),
        );
    }
    Ok(out)
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ffi::ErrorCode;
        match e {
            rusqlite::Error::SqlInputError {
                error, msg, offset, ..
            } => Self {
                offset: u32::try_from(offset).ok(),
                ..Self::new(message(error, Some(msg)))
            },
            rusqlite::Error::SqliteFailure(error, msg) => {
                let mut db = Self::new(message(error, msg));
                if error.code == ErrorCode::DatabaseBusy {
                    db.message.push_str(
                        "\nAnother window holds the file: it has uncommitted changes (commit or \
                         roll back there), or a result with rows left to fetch (fetch them all, \
                         or close it) while this one writes.",
                    );
                }
                db
            }
            rusqlite::Error::MultipleStatement => {
                Self::new("More than one statement: run them as a script (F8), or end each with ;")
            }
            e => Self::new(e.to_string()),
        }
    }
}

/// SQLite's message, which says what failed (`UNIQUE constraint failed:
/// t.id`), and its error code's name.
fn message(error: rusqlite::ffi::Error, msg: Option<String>) -> String {
    use rusqlite::ffi::ErrorCode;
    let msg = msg.unwrap_or_else(|| error.to_string());
    match error.code {
        ErrorCode::OperationInterrupted => "Cancelled".to_owned(),
        _ => msg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let t = Target::parse("/data/app.db").unwrap();
        assert_eq!(t.file, File::Path("/data/app.db".into()));
        assert!(!t.read_only && !t.create);
        let t = Target::parse("C:\\data\\app.db?ro").unwrap();
        assert_eq!(t.file, File::Path("C:\\data\\app.db".into()));
        assert!(t.read_only);
        let t = Target::parse("new.db?create").unwrap();
        assert!(t.create);
        // A `?` of the file name's own.
        let t = Target::parse("what?.db").unwrap();
        assert_eq!(t.file, File::Path("what?.db".into()));
        assert_eq!(Target::parse(":memory:").unwrap().file, File::Memory);
        let t = Target::parse("file:x.db?mode=ro&cache=private").unwrap();
        assert!(t.read_only);
        assert!(matches!(t.file, File::Uri(_)));
        assert!(Target::parse("  ").is_err());
    }

    #[test]
    fn affinities() {
        for (declared, affinity) in [
            ("INTEGER", Affinity::Integer),
            ("bigint unsigned", Affinity::Integer),
            ("VARCHAR(20)", Affinity::Text),
            ("BLOB", Affinity::Blob),
            ("", Affinity::Blob),
            ("DOUBLE PRECISION", Affinity::Real),
            ("DECIMAL(10,2)", Affinity::Numeric),
            ("DATE", Affinity::Numeric),
            // SQLite's own example: "POINT" has an "INT".
            ("FLOATING POINT", Affinity::Integer),
        ] {
            assert_eq!(super::affinity(declared), affinity, "{declared}");
        }
    }

    #[test]
    fn cells() {
        assert_eq!(cell(ValueRef::Real(1.0)).as_deref(), Some("1.0"));
        assert_eq!(cell(ValueRef::Real(0.1)).as_deref(), Some("0.1"));
        assert_eq!(cell(ValueRef::Integer(-7)).as_deref(), Some("-7"));
        assert_eq!(cell(ValueRef::Blob(&[0xca, 0xfe])).as_deref(), Some("CAFE"));
        assert_eq!(cell(ValueRef::Null), None);
    }

    #[test]
    fn date_columns_align_by_their_values() {
        let mut text = Seen::default();
        text.note(ValueRef::Text(b"2026-10-31"));
        assert!(!column("d", Some("date"), &text).numeric);
        let mut numbers = Seen::default();
        numbers.note(ValueRef::Integer(1));
        assert!(column("n", Some("numeric"), &numbers).numeric);
        assert!(column("n", Some("integer"), &Seen::default()).numeric);
        let expression = column("count(*)", None, &numbers);
        assert_eq!(expression.type_name, "INTEGER");
        assert!(expression.numeric);
    }

    #[test]
    fn binds_keep_their_text_unless_numbers() {
        assert_eq!(bind_value("7"), Value::Integer(7));
        assert_eq!(bind_value("-12"), Value::Integer(-12));
        assert_eq!(bind_value("1.5"), Value::Real(1.5));
        for text in ["007", "+7", "1e5", "7 ", "x", "", "1.50"] {
            assert_eq!(bind_value(text), Value::Text(text.into()), "{text:?}");
        }
    }

    #[test]
    fn names_in_errors() {
        let sql = "select * from emp e join \"Dept\" d on 1";
        assert_eq!(name_offset(sql, "no such table: emp"), Some(14));
        assert_eq!(name_offset(sql, "no such table: main.dept"), Some(25));
        assert_eq!(name_offset(sql, "syntax error"), None);
    }

    #[test]
    fn statements_that_change() {
        assert!(changes("insert or replace into t values (1)"));
        assert!(changes("with x as (select 1) delete from t"));
        assert!(!changes("select 1"));
        assert!(!changes("create table t (a)"));
    }
}
