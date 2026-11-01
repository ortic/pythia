use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use pythia_config::{DbKind, Privilege};

use crate::dialect::Dialect;
use crate::meta::{CompileError, MetaQuery, MetaResult};
use crate::ssh::{HostKey, Tunnel};

#[derive(Clone)]
pub struct ConnectParams {
    /// Which backend the session uses.
    pub kind: DbKind,
    pub username: String,
    pub password: String,
    pub connect_string: String,
    pub privilege: Privilege,
    /// Statements run on every new session (login script, NLS, schema).
    pub init_sql: Vec<String>,
    /// Reach the database through this SSH connection, shared by the
    /// connection's sessions (MySQL, PostgreSQL, SQL Server).
    pub ssh: Option<Arc<Tunnel>>,
}

impl ConnectParams {
    /// A connect error, explained by the tunnel when it was the cause.
    pub(crate) fn explain(&self, error: DbError) -> DbError {
        match &self.ssh {
            Some(tunnel) => tunnel.explain(error),
            None => error,
        }
    }
}

impl fmt::Debug for ConnectParams {
    // Never print the password, not even in debug logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectParams")
            .field("kind", &self.kind)
            .field("username", &self.username)
            .field("connect_string", &self.connect_string)
            .field("privilege", &self.privilege)
            .field("ssh", &self.ssh)
            .finish_non_exhaustive()
    }
}

/// A closure run on the session's worker thread with its connection. Tasks
/// are written against the Oracle driver: other databases answer them with
/// an error, so the UI only sends them where the dialect has the feature.
pub type TaskFn = Box<dyn FnOnce(&crate::oracle::Connection) -> TaskResult + Send>;
pub type TaskResult = Result<Box<dyn std::any::Any + Send>, DbError>;

/// A session's connection as tools see it, whatever the database: for work
/// that is the same everywhere but its SQL (see [`DbRequest::Run`]).
pub trait Client {
    fn dialect(&self) -> Dialect;
    /// All rows of a query as text. Parameters are positional: `:1`, `:2`
    /// on Oracle, `?` on MySQL.
    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Cell>>, DbError>;
    /// Answers a dictionary lookup as [`DbRequest::Meta`] does.
    fn meta(&mut self, query: &MetaQuery) -> Result<MetaResult, DbError>;
    /// Runs a statement without rows, e.g. DDL.
    fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        self.query(sql, &[]).map(|_| ())
    }
}

/// A closure run on the session's worker thread with its [`Client`].
pub type RunFn = Box<dyn FnOnce(&mut dyn Client) -> TaskResult + Send>;

pub enum DbRequest {
    /// Executes one statement. Queries return the first `max_rows` rows.
    Execute {
        id: u64,
        sql: String,
        max_rows: usize,
        /// Values for `:name` bind variables, bound as strings (`None` = NULL).
        binds: Vec<(String, Option<String>)>,
    },
    /// Executes a query as `Execute` does, for writing its rows into a file
    /// (*Execute to file*): binary values come whole rather than cut short
    /// to [`crate::SHOWN_BYTES`] for the grid, and a batch also ends once
    /// it holds 64 MB of text. `FetchMore` fetches the next one (#204).
    ExecuteWhole {
        id: u64,
        sql: String,
        max_rows: usize,
    },
    /// Data dictionary lookup; answered with [`DbEvent::Meta`].
    Meta {
        id: u64,
        query: MetaQuery,
    },
    /// Fetches more rows of the open result set of query `id`.
    FetchMore {
        id: u64,
        max_rows: usize,
    },
    Commit,
    Rollback,
    SetAutocommit(bool),
    /// Runs arbitrary code on the connection (used by tools such as the
    /// monitor). Answered with [`DbEvent::TaskDone`].
    Task {
        id: u64,
        task: TaskFn,
    },
    /// Runs code on any database's connection through [`Client`]. Answered
    /// with [`DbEvent::TaskDone`].
    Run {
        id: u64,
        task: RunFn,
    },
    Disconnect,
}

impl fmt::Debug for DbRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbRequest::Execute {
                id,
                sql,
                max_rows,
                binds,
            } => f
                .debug_struct("Execute")
                .field("id", id)
                .field("sql", sql)
                .field("max_rows", max_rows)
                .field("binds", &binds.len())
                .finish(),
            DbRequest::ExecuteWhole { id, sql, max_rows } => f
                .debug_struct("ExecuteWhole")
                .field("id", id)
                .field("sql", sql)
                .field("max_rows", max_rows)
                .finish(),
            DbRequest::Meta { id, query } => f
                .debug_struct("Meta")
                .field("id", id)
                .field("query", query)
                .finish(),
            DbRequest::FetchMore { id, max_rows } => f
                .debug_struct("FetchMore")
                .field("id", id)
                .field("max_rows", max_rows)
                .finish(),
            DbRequest::Commit => f.write_str("Commit"),
            DbRequest::Rollback => f.write_str("Rollback"),
            DbRequest::SetAutocommit(on) => f.debug_tuple("SetAutocommit").field(on).finish(),
            DbRequest::Task { id, .. } => f
                .debug_struct("Task")
                .field("id", id)
                .finish_non_exhaustive(),
            DbRequest::Run { id, .. } => f
                .debug_struct("Run")
                .field("id", id)
                .finish_non_exhaustive(),
            DbRequest::Disconnect => f.write_str("Disconnect"),
        }
    }
}

#[derive(Debug)]
pub enum DbEvent {
    Connected(ServerInfo),
    ConnectFailed(DbError),
    QueryStarted {
        id: u64,
        columns: Vec<Column>,
    },
    Rows {
        id: u64,
        rows: Vec<Vec<Cell>>,
        more: bool,
        elapsed: Duration,
    },
    /// Statement `id` returns another result after the rows that come
    /// next, the last of the one before: MySQL's CALL of a procedure with
    /// several SELECTs, or statements in one text (#290). Another
    /// `QueryStarted` follows, or the error that ended the statement.
    MoreResults {
        id: u64,
    },
    Executed {
        id: u64,
        summary: ExecSummary,
        elapsed: Duration,
    },
    Failed {
        id: u64,
        error: DbError,
        elapsed: Duration,
    },
    Meta {
        id: u64,
        result: Result<MetaResult, DbError>,
    },
    TaskDone {
        id: u64,
        result: TaskResult,
    },
    /// Lines read from `DBMS_OUTPUT` after a statement (Oracle).
    Output(Vec<String>),
    /// Whether the session has uncommitted changes.
    Transaction {
        pending: bool,
    },
    /// The time zone a MySQL session shows TIMESTAMP values in, as `SET
    /// time_zone` takes it: when it connects, and after a statement that
    /// changed it. INSERT statements of its results set it (#213).
    TimeZone(String),
    Notice(String),
    Error(DbError),
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub dialect: Dialect,
    pub version: String,
    /// Major and minor release, for features newer versions have.
    pub release: crate::compat::Release,
    pub banner: String,
    pub current_schema: String,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    /// Numbers are right aligned in the grid.
    pub numeric: bool,
}

/// A cell value rendered to text; `None` is SQL NULL.
pub type Cell = Option<String>;

#[derive(Debug, Clone)]
pub struct ExecSummary {
    /// Statement type, e.g. `UPDATE`, `CREATE`, `PL/SQL block`.
    pub kind: String,
    pub rows_affected: Option<u64>,
    /// For `CREATE` of a stored unit: the object and its compile errors.
    pub compiled: Option<Compiled>,
}

#[derive(Debug, Clone)]
pub struct Compiled {
    pub owner: String,
    pub name: String,
    pub object_type: String,
    pub errors: Vec<CompileError>,
}

impl fmt::Display for ExecSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(c) = &self.compiled {
            let ty = c.object_type.to_lowercase();
            let errors = c.errors.iter().filter(|e| !e.warning).count();
            let warnings = c.errors.len() - errors;
            return match (errors, warnings) {
                (0, 0) => write!(f, "{ty} {}.{} compiled", c.owner, c.name),
                (0, w) => write!(
                    f,
                    "{ty} {}.{} compiled with {w} warning(s)",
                    c.owner, c.name
                ),
                (e, _) => write!(f, "{ty} {}.{} compiled with {e} error(s)", c.owner, c.name),
            };
        }
        match self.rows_affected {
            Some(1) => write!(f, "{}: 1 row affected", self.kind),
            Some(n) => write!(f, "{}: {n} rows affected", self.kind),
            None => write!(f, "{} executed", self.kind),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct DbError {
    /// The database's error code (ORA-nnnnn, MySQL's error number), if the
    /// error came from the database.
    pub code: Option<i32>,
    /// `code` is Oracle's: [`DbError::text`] shows it as `ORA-nnnnn`.
    pub oracle: bool,
    pub message: String,
    /// Byte offset of the error in the statement text, if known.
    pub offset: Option<u32>,
    /// Set by backends that know from the error that the session is gone.
    pub lost: bool,
    /// The SSH server's host key isn't known: connecting works once the
    /// user trusts it.
    pub host_key: Option<Box<HostKey>>,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: None,
            oracle: false,
            message: message.into(),
            offset: None,
            lost: false,
            host_key: None,
        }
    }

    pub(crate) fn unknown_host_key(server: impl fmt::Display, key: HostKey) -> Self {
        let message = format!(
            "SSH: the server {server} isn't known yet. Its host key is {}. Trust it only if \
             that is the server's key.",
            key.fingerprint
        );
        Self {
            host_key: Some(Box::new(key)),
            ..Self::new(message)
        }
    }

    /// For requests a database can't serve, e.g. an Oracle task on MySQL.
    pub fn unsupported(what: &str, dialect: Dialect) -> Self {
        Self::new(format!(
            "{what} isn't available for {} connections",
            dialect.name()
        ))
    }

    /// The session is gone (network or instance failure, killed session).
    pub fn is_connection_lost(&self) -> bool {
        self.lost
            || matches!(
                self.code,
                Some(28 | 1012 | 2396 | 3113 | 3114 | 3135 | 12537 | 12570)
            )
            || [
                "DPI-1080",
                "DPI-1010",
                "ORA-03113",
                "ORA-03114",
                "ORA-03135",
            ]
            .iter()
            .any(|c| self.message.contains(c))
    }

    /// The message as shown to the user: an Oracle error starts with its
    /// `ORA-nnnnn:` (the driver's message may lack it); MySQL's already
    /// reads `ERROR nnnn (state): …`.
    pub fn text(&self) -> String {
        match self.code {
            Some(code) if self.oracle && !self.message.starts_with("ORA-") => {
                format!("ORA-{code:05}: {}", self.message)
            }
            _ => self.message.clone(),
        }
    }

    /// The PL/SQL error stack of an Oracle error: its `ORA-06512` lines.
    /// Their `text_line` counts the lines of [`DbError::text`].
    pub fn stack(&self) -> Vec<crate::error_stack::StackFrame> {
        if !self.oracle {
            return Vec::new();
        }
        crate::error_stack::parse(&self.text())
    }

    /// True if the Oracle client libraries could not be loaded (DPI-1047).
    pub fn is_missing_client(&self) -> bool {
        self.message.contains("DPI-1047")
    }
}

impl From<crate::oracle::Error> for DbError {
    fn from(e: crate::oracle::Error) -> Self {
        match e.db_error() {
            Some(db) => {
                let mut message = db.message().trim_end().to_owned();
                if db.code() == 1805 {
                    // Raised by the client while fetching, not by the database.
                    message.push_str(
                        "\nThe Oracle client's time zone file lacks a time zone region in this \
                         result (TIMESTAMP WITH TIME ZONE). Instant Client \"Basic Light\" only \
                         has the small time zone file: install \"Basic\" instead, or select the \
                         column with TO_CHAR().",
                    );
                }
                Self {
                    code: Some(db.code()).filter(|c| *c != 0),
                    oracle: true,
                    offset: Some(db.offset()).filter(|o| *o != 0),
                    ..Self::new(message)
                }
            }
            None => Self::new(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_error_text() {
        let e = DbError {
            code: Some(942),
            oracle: true,
            ..DbError::new("table or view does not exist")
        };
        assert_eq!(e.text(), "ORA-00942: table or view does not exist");
        let e = DbError {
            code: Some(942),
            oracle: true,
            ..DbError::new("ORA-00942: table or view does not exist")
        };
        assert_eq!(e.text(), "ORA-00942: table or view does not exist");
        assert_eq!(DbError::new("Not connected").text(), "Not connected");
    }
}
