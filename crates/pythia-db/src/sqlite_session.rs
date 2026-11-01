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
use std::time::{Duration, Instant};

use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags, Rows, Statement, params_from_iter};
use tracing::{debug, info, warn};

use crate::dialect::Dialect;
use crate::meta::{MetaQuery, MetaResult};
use crate::session::{CancelSlot, Canceller, Events, Requests};
use crate::sql_text::{WHOLE_BATCH_BYTES, hex, release, replace_binds, row_bytes, statement_kind};
use crate::types::{
    Cell, Client, Column, ConnectParams, DbError, DbEvent, DbRequest, ExecSummary, ServerInfo,
};

/// How long a statement waits for another session's lock on the file.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// `:memory:` is one database for all sessions of the app (each would
/// otherwise get an empty one of its own), kept while one of them is open.
const SHARED_MEMORY: &str = "file:pythia-memory?mode=memory&cache=shared";

pub(crate) fn run(params: ConnectParams, requests: Requests, events: Events, shared: CancelSlot) {
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
/// missing file is only made with `create` (a URI's `mode=rwc`), so a typo
/// doesn't leave an empty database behind.
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
            // Only `mode=rwc` makes a missing file (#266): without a
            // `mode=`, SQLite would.
            if target.create {
                flags |= OpenFlags::SQLITE_OPEN_CREATE;
            } else if let Some(path) = uri_file(uri)
                && !path.exists()
            {
                return Err(DbError::new(format!(
                    "There is no database file {} (add mode=rwc to make a new one)",
                    path.display()
                )));
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
            let mode = uri_param(text, "mode");
            return Ok(Target {
                read_only: mode == Some("ro"),
                file: File::Uri(text.to_owned()),
                create: mode == Some("rwc"),
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

/// `connect_string` without `?create`, which makes the file for the
/// connect it is given to only: kept, it made an empty database where the
/// file had been moved or deleted (#255). Other options stay, and so does
/// a `file:` URI, whose `mode=` is SQLite's.
pub fn sqlite_without_create(connect_string: &str) -> String {
    let text = connect_string.trim();
    if !text.starts_with("file:")
        && let Some((path, options)) = text.rsplit_once('?')
        && options.split('&').all(|o| Target::option(o).is_some())
    {
        let kept: Vec<&str> = options
            .split('&')
            .filter(|o| !matches!(Target::option(o), Some(Opt::Create)))
            .collect();
        if kept.is_empty() {
            return path.to_owned();
        }
        if kept.len() < options.split('&').count() {
            return format!("{path}?{}", kept.join("&"));
        }
    }
    connect_string.to_owned()
}

/// The value of a `file:` URI's parameter `key`, as written.
fn uri_param<'a>(uri: &'a str, key: &str) -> Option<&'a str> {
    let (_, query) = uri.split('#').next()?.split_once('?')?;
    query
        .split('&')
        .find_map(|p| p.split_once('=').filter(|(k, _)| *k == key))
        .map(|(_, value)| value)
}

/// The file a `file:` URI opens, as SQLite reads it: the path after an
/// empty or `localhost` authority, up to the parameters, `%HH` decoded.
/// None where no file of this machine is named: memory, a temporary
/// database (no path), another host or VFS.
fn uri_file(uri: &str) -> Option<PathBuf> {
    if uri_param(uri, "mode") == Some("memory") || uri_param(uri, "vfs").is_some() {
        return None;
    }
    let rest = uri.strip_prefix("file:")?;
    let mut path = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    if let Some(after) = path.strip_prefix("//") {
        let (authority, rest) = after.split_at(after.find('/').unwrap_or(after.len()));
        if !matches!(authority, "" | "localhost") {
            return None;
        }
        path = rest;
    }
    // `/C:/…` on Windows is the drive's.
    if cfg!(windows) && path.as_bytes().get(2) == Some(&b':') && path.starts_with('/') {
        path = &path[1..];
    }
    let bytes = path.as_bytes();
    let hex = |i: usize| {
        let digit = |b: Option<&u8>| b.and_then(|&b| (b as char).to_digit(16));
        Some(digit(bytes.get(i + 1))? * 16 + digit(bytes.get(i + 2))?)
    };
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match hex(i).filter(|_| bytes[i] == b'%') {
            Some(byte) => {
                decoded.push(byte as u8);
                i += 3;
            }
            None => {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    (!path.is_empty() && path != ":memory:").then(|| PathBuf::from(path))
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
    /// returns the request that closed it, if any. Blobs come `whole` or
    /// cut short for the grid.
    fn execute(
        &mut self,
        id: u64,
        sql: &str,
        max_rows: usize,
        binds: &[(String, Option<String>)],
        requests: &Requests,
        whole: bool,
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
            whole,
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
    /// Blobs whole, for a file (#204).
    whole: bool,
    started: Instant,
    events: &'a Events,
    requests: &'a Requests,
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
            let mut bytes = 0;
            while batch.len() < max_rows && bytes < WHOLE_BATCH_BYTES {
                match rows.next() {
                    Ok(Some(row)) => {
                        let mut cells = Vec::with_capacity(names.len());
                        for i in 0..names.len() {
                            let value = row.get_ref(i).unwrap_or(ValueRef::Null);
                            if let Some(seen) = seen.get_mut(i) {
                                seen.note(value);
                            }
                            cells.push(cell(value, self.whole));
                        }
                        if self.whole {
                            bytes += row_bytes(&cells);
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

    /// A value that isn't NULL.
    fn any(self) -> bool {
        self.integer || self.real || self.text || self.blob
    }

    /// The storage class of the values: BLOB if one is a blob, else the
    /// one most of them had.
    fn type_name(self) -> &'static str {
        match self {
            Seen { blob: true, .. } => "BLOB",
            Seen { text: true, .. } => "TEXT",
            Seen { real: true, .. } => "REAL",
            Seen { integer: true, .. } => "INTEGER",
            _ => "",
        }
    }
}

/// A result column: its declared type, or the values' when it has none (an
/// expression) or they say otherwise about blobs (#209): blobs in an
/// `image` or `binary(16)` column make it BLOB, and a BLOB column of text
/// is TEXT, so that exports and edits write each as what it is. Numbers
/// are right aligned: a column with INTEGER or REAL affinity, or one whose
/// values were all numbers.
fn column(name: &str, declared: Option<&str>, seen: &Seen) -> Column {
    let type_name = match declared.map(str::to_ascii_uppercase) {
        Some(declared) if !seen.any() || seen.blob == declared.contains("BLOB") => declared,
        _ => seen.type_name().to_owned(),
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
/// blobs in hex, cut short unless `whole`.
pub(crate) fn cell(value: ValueRef<'_>, whole: bool) -> Cell {
    Some(match value {
        ValueRef::Null => return None,
        ValueRef::Integer(i) => i.to_string(),
        // `{:?}`: shortest round trip, keeps `.0`, exponent for large and
        // tiny values.
        ValueRef::Real(f) => format!("{f:?}"),
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        ValueRef::Blob(bytes) => hex(bytes, whole),
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
                .map(|i| cell(row.get_ref(i).unwrap_or(ValueRef::Null), false))
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
        assert!(t.read_only && !t.create);
        assert!(matches!(t.file, File::Uri(_)));
        let t = Target::parse("file:x.db?cache=private&mode=rwc").unwrap();
        assert!(t.create && !t.read_only);
        assert!(!Target::parse("file:x.db?mode=rw").unwrap().create);
        assert!(Target::parse("  ").is_err());
    }

    /// The file a URI names, which must be there unless `mode=rwc` (#266).
    #[test]
    fn files_of_uris() {
        for (uri, file) in [
            ("file:x.db", Some("x.db")),
            ("file:/data/x.db?cache=private", Some("/data/x.db")),
            ("file:///data/x.db#top", Some("/data/x.db")),
            ("file://localhost/data/my%20x.db", Some("/data/my x.db")),
            ("file:data%3f.db?mode=rw", Some("data?.db")),
            ("file://elsewhere/data/x.db", None),
            ("file:x.db?mode=memory", None),
            ("file::memory:", None),
            ("file:?mode=ro", None),
            ("file:x.db?vfs=memdb", None),
        ] {
            assert_eq!(uri_file(uri), file.map(PathBuf::from), "{uri}");
        }
    }

    /// What a connection keeps once its file is made (#255).
    #[test]
    fn create_is_for_one_connect() {
        for (given, kept) in [
            ("/data/app.db?create", "/data/app.db"),
            ("C:\\data\\app.db?ro&create", "C:\\data\\app.db?ro"),
            ("app.db?Create&ro", "app.db?ro"),
            ("sqlite://app.db?mode=rwc", "sqlite://app.db"),
            ("/data/app.db?ro", "/data/app.db?ro"),
            ("/data/app.db", "/data/app.db"),
            // A `?` of the file name's own, a URI's mode, memory.
            ("what?create.db", "what?create.db"),
            ("file:x.db?mode=rwc", "file:x.db?mode=rwc"),
            (":memory:", ":memory:"),
        ] {
            assert_eq!(sqlite_without_create(given), kept, "{given}");
        }
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
        assert_eq!(cell(ValueRef::Real(1.0), false).as_deref(), Some("1.0"));
        assert_eq!(cell(ValueRef::Real(0.1), false).as_deref(), Some("0.1"));
        assert_eq!(cell(ValueRef::Integer(-7), false).as_deref(), Some("-7"));
        assert_eq!(
            cell(ValueRef::Blob(&[0xca, 0xfe]), false).as_deref(),
            Some("CAFE")
        );
        assert_eq!(cell(ValueRef::Null, false), None);
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

    /// Whether a column holds blobs is the values' to say, not the
    /// declared type's: blobs in an `image` column are blobs, text in a
    /// BLOB column is text; without values, the declared type decides.
    #[test]
    fn blob_columns_by_their_values() {
        let seen = |values: &[ValueRef<'_>]| {
            let mut seen = Seen::default();
            for v in values {
                seen.note(*v);
            }
            seen
        };
        let blobs = seen(&[ValueRef::Blob(b"\xca\xfe"), ValueRef::Null]);
        let text = seen(&[ValueRef::Text(b"hello world")]);
        let mixed = seen(&[ValueRef::Text(b"x"), ValueRef::Blob(b"1")]);
        let numbers = seen(&[ValueRef::Integer(7)]);
        let none = Seen::default();
        for (declared, seen, type_name) in [
            (Some("image"), &blobs, "BLOB"),
            (Some("binary(16)"), &blobs, "BLOB"),
            (None, &blobs, "BLOB"),
            (Some("BLOB"), &blobs, "BLOB"),
            (Some("blob"), &text, "TEXT"),
            (Some("BLOB"), &numbers, "INTEGER"),
            (Some("BLOB"), &none, "BLOB"),
            (Some("image"), &text, "IMAGE"),
            (Some("image"), &none, "IMAGE"),
            (Some("text"), &mixed, "BLOB"),
            (None, &mixed, "BLOB"),
        ] {
            let col = column("c", declared, seen);
            assert_eq!(col.type_name, type_name, "{declared:?} {seen:?}");
            assert_eq!(col.numeric, type_name == "INTEGER", "{declared:?} {seen:?}");
        }
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
