//! Database connectivity (Oracle, MySQL, PostgreSQL, SQL Server), directly
//! or through SSH.
//!
//! Each [`Session`] owns a worker thread with one database connection. The UI
//! never touches the connection directly: it sends [`DbRequest`]s and polls
//! [`DbEvent`]s, so a slow query can never block a frame. The only call that
//! bypasses the queue is [`Session::cancel`], which has to interrupt the
//! worker while it is busy.
//!
//! Requests and events are the same for every database; a connection's
//! [`Dialect`] tells what its database supports.

pub mod compat;
mod dialect;
pub mod error_stack;
mod meta;
mod mssql_meta;
mod mssql_session;
mod mysql_meta;
mod mysql_session;
mod pg_meta;
mod pg_session;
pub mod recompile;
mod session;
mod sql_text;
pub mod ssh;
mod types;

pub use dialect::{DbKind, Dialect, Feature};
pub use meta::{
    CompileError, DbObject, Detail, Member, MetaQuery, MetaResult, ObjectType, TableColumn,
    cursor_plan, display_cursor, like_pattern, object_ddl, object_source,
};
pub use session::{Session, open_connection, stream_query};
pub use types::{
    Cell, Client, Column, Compiled, ConnectParams, DbError, DbEvent, DbRequest, ExecSummary, RunFn,
    ServerInfo, TaskFn, TaskResult,
};

/// Installs ring as rustls' process-wide crypto provider, once. A driver
/// that takes the default (oracledb's `ClientConfig::builder()`) panics
/// when more than one provider is compiled in and none is installed (#104).
pub(crate) fn install_crypto_provider() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        // One installed before (by a program embedding this crate) stays.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Test runs against the shared test databases (a `PYTHIA_TEST_*` variable
/// set) wait for each other: the first connection takes a machine-wide lock
/// that the process keeps until it ends, so two runs (two worktrees) don't
/// fail each other's tests on the same objects (#100). A program, without
/// those variables, never locks.
pub(crate) fn wait_for_other_test_runs() {
    static LOCK: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| {
        let testing =
            std::env::vars_os().any(|(name, _)| name.to_string_lossy().starts_with("PYTHIA_TEST_"));
        testing.then(|| take_test_lock(&std::env::temp_dir().join("pythia-test-databases.lock")))?
    });
}

/// The lock at `path`, waiting for its holder if there is one.
fn take_test_lock(path: &std::path::Path) -> Option<std::fs::File> {
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    if file.try_lock().is_err() {
        eprintln!(
            "Another test run uses the test databases: waiting for it ({})",
            path.display()
        );
        file.lock().ok()?;
    }
    Some(file)
}

/// The database driver, for tools running [`DbRequest::Task`]: the `oracle`
/// API on the pure-Rust `oracledb` driver (no client libraries needed), or
/// with the `thick` feature the `oracle` crate itself (ODPI-C, needs the
/// Oracle Instant Client).
#[cfg(feature = "thick")]
pub use ::oracle;
#[cfg(not(feature = "thick"))]
#[path = "thin.rs"]
pub mod oracle;

/// Built with the `thick` feature: Oracle through ODPI-C and the Oracle
/// Client.
pub const THICK: bool = cfg!(feature = "thick");

/// How this build reaches Oracle, for the About dialog. With the `thick`
/// feature the first call loads the Oracle Client to ask for its version.
pub fn oracle_driver() -> &'static str {
    static DRIVER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DRIVER.get_or_init(|| {
        #[cfg(feature = "thick")]
        let driver = match ::oracle::Version::client() {
            Ok(v) => format!("Oracle Client {v}"),
            Err(_) => "Oracle Client (not found)".to_owned(),
        };
        #[cfg(not(feature = "thick"))]
        let driver = "Pure-Rust driver (no Oracle client)".to_owned();
        driver
    })
}

#[cfg(test)]
mod tests {
    /// Without a default provider, `builder()` panics when both ring and
    /// aws-lc-rs are compiled in (#104).
    #[test]
    fn rustls_has_a_default_provider() {
        super::install_crypto_provider();
        let _ = rustls::ClientConfig::builder();
    }

    /// Held, the test lock keeps another holder out; dropped, it's free.
    #[test]
    fn test_lock() {
        let path = std::env::temp_dir().join(format!("pythia-test-lock-{}", std::process::id()));
        let held = super::take_test_lock(&path).expect("the lock");
        let other = std::fs::File::options().write(true).open(&path).unwrap();
        assert!(other.try_lock().is_err(), "a second holder got in");
        drop(held);
        assert!(other.try_lock().is_ok());
        drop(other);
        let _ = std::fs::remove_file(&path);
    }
}
