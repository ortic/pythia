//! The target side: running code under the debugger, compiling units with
//! debug info, and the admin setup script.

use pythia_db::oracle::Connection;
use pythia_db::{DbError, TaskFn};

use crate::types::{Unit, UnitType};

/// Connects the session to the debugger, runs `block`, then disconnects.
///
/// Blocks for the whole debug session, so run it on a dedicated session
/// (e.g. as a `DbRequest::Task`). The block's own error is returned; a
/// terminated session fails with ORA-30687. With `statistics` the session
/// collects row source statistics meanwhile (see [`debug_task`]).
pub fn run_under_debugger(
    conn: &Connection,
    host: &str,
    port: u16,
    block: &str,
    statistics: bool,
) -> Result<(), DbError> {
    run(conn, host, port, block, statistics).1
}

/// [`run_under_debugger`], also saying whether the session connected to
/// the debugger (`CONNECT_TCP` failing is a setup problem, see
/// [`crate::setup::Problem`]).
fn run(
    conn: &Connection,
    host: &str,
    port: u16,
    block: &str,
    statistics: bool,
) -> (bool, Result<(), DbError>) {
    // Compiled at level 1 the block itself has debug information, so it
    // can be stepped through (and stepping out of a unit lands in it).
    let level = optimize_level(conn);
    if let Err(e) = conn.execute("alter session set plsql_optimize_level = 1", &[]) {
        return (false, Err(e.into()));
    }
    // Actual rows, times and reads per plan line of every statement the
    // code runs, for the SQL panel ([`crate::sql`]).
    let statistics_level = statistics
        .then(|| parameter(conn, "statistics_level"))
        .flatten()
        .filter(|level| !level.eq_ignore_ascii_case("ALL"));
    if statistics_level.is_some() {
        let _ = conn.execute("alter session set statistics_level = all", &[]);
    }
    let connected = conn
        .execute(
            "begin dbms_debug_jdwp.connect_tcp(:1, :2); end;",
            &[&host, &i32::from(port)],
        )
        .map_err(DbError::from);
    let result = connected.as_ref().map_err(Clone::clone).and_then(|_| {
        let result = conn.execute(block, &[]).map(drop).map_err(DbError::from);
        // The session may be gone if the debugger terminated it.
        let _ = conn.execute("begin dbms_debug_jdwp.disconnect; end;", &[]);
        result
    });
    let _ = conn.execute(
        &format!("alter session set plsql_optimize_level = {level}"),
        &[],
    );
    if let Some(level) = statistics_level.filter(|l| l.chars().all(|c| c.is_ascii_alphabetic())) {
        let _ = conn.execute(
            &format!("alter session set statistics_level = {level}"),
            &[],
        );
    }
    (connected.is_ok(), result)
}

/// The session's value of a parameter, if `V$PARAMETER` is readable.
fn parameter(conn: &Connection, name: &str) -> Option<String> {
    conn.query_row_as::<Option<String>>("select value from v$parameter where name = :1", &[&name])
        .ok()
        .flatten()
}

/// The session's `PLSQL_OPTIMIZE_LEVEL`, or Oracle's default (2) when
/// `V$PARAMETER` isn't readable.
fn optimize_level(conn: &Connection) -> u8 {
    parameter(conn, "plsql_optimize_level")
        .and_then(|v| v.trim().parse().ok())
        .filter(|l| *l <= 3)
        .unwrap_or(2)
}

/// How a debugged block ended: what it printed with DBMS_OUTPUT and its own
/// error, if any.
#[derive(Debug)]
pub struct DebugOutcome {
    pub output: Vec<String>,
    /// The session connected to the debugger. If not, `error` is why
    /// `CONNECT_TCP` failed and the debugger is still waiting for it.
    pub connected: bool,
    pub error: Option<DbError>,
}

/// [`run_under_debugger`] as a task for `pythia_db::DbRequest::Task`. The task
/// result is a [`DebugOutcome`] (the block's error is part of it, so its
/// output isn't lost).
///
/// With `statistics` the session runs with `STATISTICS_LEVEL = ALL`, so
/// the plans of the statements it runs have actual rows next to the
/// estimates. That costs some time per row, and the statements get child
/// cursors of their own (`STATS_ROW_MISMATCH`), parsed with this run's
/// binds.
pub fn debug_task(host: String, port: u16, block: String, statistics: bool) -> TaskFn {
    Box::new(move |conn: &Connection| {
        let (connected, result) = run(conn, &host, port, &block, statistics);
        let output = read_output(conn).unwrap_or_default();
        Ok(Box::new(DebugOutcome {
            output,
            connected,
            error: result.err(),
        }) as Box<dyn std::any::Any + Send>)
    })
}

/// A task for the session about to be debugged, sent before
/// [`debug_task`]: who the session is, so its SQL can be looked up from
/// another session while it is stopped. The result is a
/// [`crate::sql::TargetSession`].
pub fn session_task() -> TaskFn {
    Box::new(|conn: &Connection| {
        let (sid, started): (u32, String) = conn.query_row_as(
            "select to_number(sys_context('userenv', 'sid')), \
                    to_char(sysdate, 'yyyy-mm-dd hh24:mi:ss') from dual",
            &[],
        )?;
        Ok(Box::new(crate::sql::TargetSession { sid, started }) as Box<dyn std::any::Any + Send>)
    })
}

/// Drains the session's DBMS_OUTPUT buffer.
pub fn read_output(conn: &Connection) -> Result<Vec<String>, DbError> {
    use pythia_db::oracle::sql_type::OracleType;
    let mut stmt = conn
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

/// SQL to recompile a unit with debug information.
pub fn compile_debug_sql(unit: &Unit) -> Option<String> {
    let name = format!("\"{}\".\"{}\"", unit.owner, unit.name);
    Some(match unit.unit_type {
        UnitType::Procedure => format!("alter procedure {name} compile debug"),
        UnitType::Function => format!("alter function {name} compile debug"),
        UnitType::Package => format!("alter package {name} compile debug package"),
        UnitType::PackageBody => format!("alter package {name} compile debug body"),
        UnitType::Trigger => format!("alter trigger {name} compile debug"),
        UnitType::Type => format!("alter type {name} compile debug specification"),
        UnitType::TypeBody => format!("alter type {name} compile debug body"),
        UnitType::Block => return None,
    })
}

/// Recompiles `unit` with debug information.
pub fn compile_debug(conn: &Connection, unit: &Unit) -> Result<(), DbError> {
    if let Some(sql) = compile_debug_sql(unit) {
        conn.execute(&sql, &[])?;
    }
    Ok(())
}

/// Recompiles `unit` with debug information unless it has it already
/// (recompiling discards the package state of every session using it).
/// Returns whether it was compiled.
pub fn ensure_debug_info(conn: &Connection, unit: &Unit) -> Result<bool, DbError> {
    if unit.unit_type == UnitType::Block || has_debug_info(conn, unit)? {
        return Ok(false);
    }
    compile_debug(conn, unit)?;
    Ok(true)
}

/// Sets whether the session's `CREATE` statements compile PL/SQL with debug
/// information (`PLSQL_DEBUG`, as `ALTER … COMPILE DEBUG` does). Returns
/// the previous setting, to restore afterwards.
///
/// Oracle has deprecated the parameter in favor of `PLSQL_OPTIMIZE_LEVEL =
/// 1`, which gives the debugger lines and variables too, but only
/// `PLSQL_DEBUG` is recorded as debug information (`ALL_PLSQL_OBJECT_SETTINGS`).
/// With PL/SQL warnings enabled, units compiled this way get PLW-06015 and
/// PLW-06013 about it ([`is_debug_notice`]).
pub fn set_compile_debug(conn: &Connection, on: bool) -> Result<bool, DbError> {
    let previous = conn
        .query_row_as::<String>(
            "select value from v$parameter where name = 'plsql_debug'",
            &[],
        )
        .is_ok_and(|v| v.eq_ignore_ascii_case("TRUE"));
    conn.execute(&format!("alter session set plsql_debug = {on}"), &[])?;
    Ok(previous)
}

/// Whether a compiler message is one of the warnings Oracle adds to every
/// unit compiled with `PLSQL_DEBUG` (that the parameter is deprecated and
/// lowers the optimization level), which say nothing about the code.
pub fn is_debug_notice(text: &str) -> bool {
    text.starts_with("PLW-06015") || text.starts_with("PLW-06013")
}

/// Whether a stored unit was compiled with debug information.
pub fn has_debug_info(conn: &Connection, unit: &Unit) -> Result<bool, DbError> {
    let flag: Option<String> = conn.query_row_as(
        "select max(plsql_debug) from all_plsql_object_settings where owner = :1 and name = :2 and type = :3",
        &[&unit.owner, &unit.name, &unit.unit_type.dictionary_name()],
    )?;
    Ok(flag.as_deref() == Some("TRUE"))
}

/// Why variables of `unit` stopped at `line` come without contents, for
/// those of `names` with a reason Pythia can name: their type is declared
/// in a package compiled without debug information, so Oracle's agent
/// hands the value over as an opaque object. `source` is the unit's text
/// if the database doesn't store it (an anonymous block). Returns
/// (variable name, reason).
pub fn opaque_reasons(
    conn: &Connection,
    unit: &Unit,
    line: Option<u32>,
    names: &[String],
    source: Option<&str>,
) -> Result<Vec<(String, String)>, DbError> {
    let source = match source {
        Some(s) => s.to_owned(),
        None => stored_source(conn, unit)?,
    };
    let mut reasons = Vec::new();
    for (name, path) in declared_types(&source, line, names) {
        // `pkg.type` or `owner.pkg.type`; one part is a local or SQL type.
        let (owner, package, type_name) = match path.as_slice() {
            [package, type_name] => (unit.owner.as_str(), package, type_name),
            [owner, package, type_name] => (owner.as_str(), package, type_name),
            _ => continue,
        };
        let without_debug_info: i64 = conn.query_row_as(
            "select count(*) from all_plsql_object_settings where owner = :1 and name = :2 \
             and type = 'PACKAGE' and nvl(plsql_debug, 'FALSE') <> 'TRUE'",
            &[&owner, package],
        )?;
        if without_debug_info > 0 {
            let package = if owner == unit.owner {
                package.clone()
            } else {
                format!("{owner}.{package}")
            };
            reasons.push((
                name,
                format!("{type_name} is declared in {package}, compiled without debug information"),
            ));
        }
    }
    Ok(reasons)
}

/// A stored unit's source as a `CREATE` statement, with the dictionary's
/// line numbers.
fn stored_source(conn: &Connection, unit: &Unit) -> Result<String, DbError> {
    let lines = conn.query_as::<Option<String>>(
        "select text from all_source where owner = :1 and name = :2 and type = :3 order by line",
        &[&unit.owner, &unit.name, &unit.unit_type.dictionary_name()],
    )?;
    let mut source = String::from("create or replace ");
    for line in lines {
        source.push_str(line?.as_deref().unwrap_or("\n"));
    }
    Ok(source)
}

/// The declared types (dotted, as Oracle stores names) of the variables,
/// constants and parameters `names` in scope at `line` of `source`.
/// `%TYPE` and `%ROWTYPE` declarations are left out.
fn declared_types(source: &str, line: Option<u32>, names: &[String]) -> Vec<(String, Vec<String>)> {
    use pythia_plsql::ast::{SymbolKind, TypeAttr};
    let ast = pythia_plsql::parse(source);
    // The first non-blank character of the line, to be inside its scope.
    let offset = line.map_or(0, |line| {
        let start: usize = source
            .split_inclusive('\n')
            .take(line.saturating_sub(1) as usize)
            .map(str::len)
            .sum();
        let indent = source[start..].len() - source[start..].trim_start().len();
        start + indent
    });
    let scope = ast.scope_at(offset);
    names
        .iter()
        .filter_map(|name| {
            let symbol = ast
                .ancestors(scope)
                .flat_map(|s| ast.scope(s).symbols.iter())
                .find(|s| {
                    s.name.is(name)
                        && matches!(
                            s.kind,
                            SymbolKind::Variable | SymbolKind::Constant | SymbolKind::Parameter(_)
                        )
                })?;
            let ty = symbol.ty.as_ref().filter(|t| t.attr == TypeAttr::None)?;
            Some((
                name.clone(),
                ty.path.iter().map(|i| i.name.clone()).collect(),
            ))
        })
        .collect()
}

/// The SQL an administrator runs once so `user` can debug, with the
/// database connecting back to `host` (the machine running Pythia, as seen
/// from the database server) on any port. Each statement is executable as
/// is. The host ACE needs 12.1+ (and isn't needed before).
pub fn setup_statements(user: &str, host: &str) -> Vec<String> {
    crate::setup::Grant {
        user: user.to_uppercase(),
        debug_connect: true,
        debug_any: true,
        ace: Some((host.to_owned(), crate::setup::ANY_PORT)),
    }
    .statements()
}

/// [`setup_statements`] as a SQL*Plus script for a DBA to run.
pub fn setup_script(user: &str, host: &str) -> String {
    format!(
        "-- Allow {} to debug PL/SQL with Pythia (run as a DBA)\n{}",
        user.to_uppercase(),
        crate::setup::script(&setup_statements(user, host))
    )
}

/// A host name the database can use to reach this machine, for
/// `DBMS_DEBUG_JDWP.CONNECT_TCP`. Prefers `PYTHIA_DEBUG_HOST`, then the
/// machine's host name. Databases in a local container usually need
/// `host.containers.internal` / `host.docker.internal` instead (the setup
/// wizard finds one that works, see [`crate::setup::candidates`]).
pub fn default_host() -> String {
    if let Ok(host) = std::env::var("PYTHIA_DEBUG_HOST")
        && !host.is_empty()
    {
        return host;
    }
    crate::setup::host_name().unwrap_or_else(|| "localhost".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_sql() {
        let unit = Unit::new("HR", "PKG", UnitType::PackageBody);
        assert_eq!(
            compile_debug_sql(&unit).unwrap(),
            "alter package \"HR\".\"PKG\" compile debug body"
        );
        assert!(compile_debug_sql(&Unit::new("HR", "x", UnitType::Block)).is_none());
    }

    #[test]
    fn declared_types_in_scope() {
        let source = "create or replace package body api as
  procedure process(p_order in out nocopy types.t_order, p_id number) is
    l_order hr.types.t_order;
    l_count pls_integer;
    l_row orders%rowtype;
  begin
    null;
  end;
  procedure other is
    l_order local_t;
  begin
    null;
  end;
end;";
        let names: Vec<String> = ["P_ORDER", "P_ID", "L_ORDER", "L_COUNT", "L_ROW", "MISSING"]
            .map(String::from)
            .into();
        let path = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            declared_types(source, Some(7), &names),
            [
                ("P_ORDER".to_string(), path(&["TYPES", "T_ORDER"])),
                ("P_ID".to_string(), path(&["NUMBER"])),
                ("L_ORDER".to_string(), path(&["HR", "TYPES", "T_ORDER"])),
                ("L_COUNT".to_string(), path(&["PLS_INTEGER"])),
            ]
        );
        // The other procedure's own L_ORDER.
        assert_eq!(
            declared_types(source, Some(12), &names[2..3]),
            [("L_ORDER".to_string(), path(&["LOCAL_T"]))]
        );
    }

    #[test]
    fn debug_notices() {
        assert!(is_debug_notice(
            "PLW-06015: parameter PLSQL_DEBUG is deprecated; use PLSQL_OPTIMIZE_LEVEL = 1"
        ));
        assert!(is_debug_notice(
            "PLW-06013: deprecated parameter PLSQL_DEBUG forces PLSQL_OPTIMIZE_LEVEL <= 1"
        ));
        assert!(!is_debug_notice(
            "PLW-07206: analysis suggests that the assignment to 'L' may be unnecessary"
        ));
    }

    #[test]
    fn setup() {
        let sql = setup_script("pythia", "dev-box");
        assert!(sql.contains("grant debug connect session to PYTHIA"));
        assert!(sql.contains("host       => 'dev-box'"));
    }

    #[test]
    fn class_signatures() {
        let unit = Unit::new("PYTHIA", "DBG_DEMO", UnitType::Procedure);
        assert_eq!(
            unit.class_signature(),
            "L$Oracle/Procedure/PYTHIA/DBG_DEMO;"
        );
        assert_eq!(
            Unit::from_class_signature(&unit.class_signature()),
            Some(unit)
        );
        let block =
            Unit::from_class_signature("L$Oracle/Block/PYTHIA/BexqMksmIPthdGZpJ4tGJwg;").unwrap();
        assert_eq!(block.unit_type, UnitType::Block);
        assert_eq!(Unit::from_class_signature("Ljava/lang/String;"), None);
    }
}
