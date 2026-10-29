//! The SQL the debugged session ran, each statement with the line of code
//! it came from, and a statement's plan with what its last execution
//! actually did: rows, time and reads per plan line next to the estimates.
//!
//! The lookups run on another session, as the debugged one is busy while
//! stopped. They need SELECT_CATALOG_ROLE, or SELECT on V$OPEN_CURSOR,
//! V$SQL and V$SQL_PLAN_STATISTICS_ALL. Static SQL stays open in the
//! session's PL/SQL cursor cache after it ran, with `V$SQL.PROGRAM_ID` and
//! `PROGRAM_LINE#` naming the unit and line that parsed it (`PROGRAM_ID` 0
//! for an anonymous block, the line still the block's).

use std::collections::HashMap;

use pythia_db::oracle::Connection;
use pythia_db::{DbError, MetaResult};
use pythia_ui::plan::PlanRow;

use crate::types::{Unit, UnitType};

/// The debugged session, from [`crate::target::session_task`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSession {
    pub sid: u32,
    /// When the debug run started, in database time (`yyyy-mm-dd
    /// hh24:mi:ss`): statements the session hasn't run since (give or take
    /// half a minute) are left out.
    pub started: String,
}

/// A statement the debugged session ran from PL/SQL.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSql {
    pub sql_id: String,
    /// The session's child cursor.
    pub child: i64,
    pub plan_hash: i64,
    /// The unit it came from, `None` for an anonymous block.
    pub unit: Option<Unit>,
    /// Where the statement starts in the unit.
    pub line: u32,
    /// The statement as PL/SQL hands it to SQL: static SQL in upper case
    /// with variables as binds (`:B1`), dynamic SQL as written.
    pub text: String,
    /// Executions of the child, by every session using it.
    pub executions: i64,
    /// Elapsed time of those executions, in seconds.
    pub elapsed: f64,
    pub buffer_gets: i64,
    /// An execution is under way: a cursor still being fetched, e.g.
    /// stopped inside its FOR loop.
    pub running: bool,
    /// When the session last ran it (database time, `hh24:mi:ss`).
    pub last_active: String,
}

/// The statements `session` ran from PL/SQL since the debug run started:
/// those still running first, then the most recent (to the second).
/// Statements of Oracle's own packages are left out.
pub fn session_sql(conn: &Connection, session: &TargetSession) -> Result<Vec<SessionSql>, DbError> {
    // The session's cursors, then each one's child in V$SQL by SQL_ID:
    // joining the two views scans the whole shared pool (a minute and
    // more). V$OPEN_CURSOR names the session's child since 12.2 (else the
    // most recently active child stands for it); its LAST_SQL_ACTIVE_TIME
    // comes from a coarse clock that lags SYSDATE by seconds.
    let open = |child: &str| -> Result<Vec<(String, Option<String>, String)>, DbError> {
        let sql = format!(
            "select sql_id, {child}, to_char(last_sql_active_time, 'yyyy-mm-dd hh24:mi:ss') \
               from v$open_cursor \
              where sid = :1 and cursor_type like '%PL/SQL%' and sql_id is not null \
                and last_sql_active_time >= to_date(:2, 'yyyy-mm-dd hh24:mi:ss') - 30 / 86400"
        );
        Ok(conn
            .query_as::<(String, Option<String>, String)>(&sql, &[&session.sid, &session.started])?
            .collect::<Result<_, _>>()?)
    };
    let cursors = match open("rawtohex(child_address)") {
        Err(e) if e.code == Some(904) => open("null")?,
        cursors => cursors?,
    };
    struct Row {
        sql_id: String,
        child: i64,
        plan_hash: i64,
        program: i64,
        line: i64,
        text: Option<String>,
        executions: i64,
        elapsed: f64,
        gets: i64,
        users: i64,
        /// When the session last ran it, then when any session did.
        last: (String, String),
    }
    let mut rows: Vec<Row> = Vec::new();
    for (sql_id, address, last) in cursors {
        if rows.iter().any(|r| r.sql_id == sql_id) {
            continue;
        }
        let children = conn.query(
            "select child_number, plan_hash_value, program_id, program_line#, sql_text, \
                    executions, elapsed_time / 1000000, buffer_gets, users_executing, \
                    rawtohex(child_address), to_char(last_active_time, 'yyyy-mm-dd hh24:mi:ss') \
               from v$sql where sql_id = :1 and command_type <> 47 \
              order by last_active_time desc",
            &[&sql_id],
        )?;
        let mut first = None;
        for child in children {
            let child = child?;
            let matches = match &address {
                Some(address) => child.get::<_, Option<String>>(9)?.as_ref() == Some(address),
                None => true,
            };
            if matches {
                first = Some(child);
                break;
            }
        }
        let Some(child) = first else {
            continue;
        };
        rows.push(Row {
            sql_id,
            child: child.get(0)?,
            plan_hash: child.get(1)?,
            program: child.get(2)?,
            line: child.get(3)?,
            text: child.get(4)?,
            executions: child.get(5)?,
            elapsed: child.get(6)?,
            gets: child.get(7)?,
            users: child.get(8)?,
            last: (
                last,
                child.get::<_, Option<String>>(10)?.unwrap_or_default(),
            ),
        });
    }
    rows.sort_by(|a, b| {
        (b.users > 0)
            .cmp(&(a.users > 0))
            .then_with(|| b.last.cmp(&a.last))
    });
    let release = pythia_db::compat::release(conn)?;
    let mut units: HashMap<i64, Option<Unit>> = HashMap::new();
    let mut out = Vec::new();
    for Row {
        sql_id,
        child,
        plan_hash,
        program,
        line,
        text,
        executions,
        elapsed,
        gets,
        users,
        last,
    } in rows
    {
        let unit = if program == 0 {
            None
        } else {
            let unit = match units.get(&program) {
                Some(unit) => unit.clone(),
                None => {
                    let unit = program_unit(conn, program, release)?;
                    units.insert(program, unit.clone());
                    unit
                }
            };
            // Oracle's own code, or code this user can't see.
            let Some(unit) = unit else {
                continue;
            };
            Some(unit)
        };
        out.push(SessionSql {
            sql_id,
            child,
            plan_hash,
            unit,
            line: line.max(0) as u32,
            text: text.unwrap_or_default(),
            executions,
            elapsed,
            buffer_gets: gets,
            running: users > 0,
            last_active: last.0.get(11..).unwrap_or_default().to_owned(),
        });
    }
    Ok(out)
}

/// The unit with object id `id`, unless Oracle maintains its schema.
fn program_unit(
    conn: &Connection,
    id: i64,
    release: pythia_db::compat::Release,
) -> Result<Option<Unit>, DbError> {
    let sql = format!(
        "select owner, object_name, object_type from all_objects \
          where object_id = :1 and owner not in ({})",
        pythia_db::compat::schemas_query(release, true)
    );
    let found = conn
        .query_as::<(String, String, String)>(&sql, &[&id])?
        .next()
        .transpose()?;
    Ok(found.and_then(|(owner, name, kind)| {
        UnitType::from_dictionary(&kind).map(|t| Unit::new(owner, name, t))
    }))
}

/// A statement's plan with the row source statistics of its last
/// execution.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorPlan {
    pub sql_id: String,
    pub child: i64,
    /// The plan lines. They have statistics only after an execution with
    /// statistics finished ([`CursorPlan::measured`]).
    pub rows: Vec<PlanRow>,
    /// The whole statement.
    pub text: String,
    pub executions: i64,
    /// An execution is under way.
    pub running: bool,
    /// Children of the statement with another plan, which other sessions
    /// may run: (child, plan hash value, executions).
    pub other_plans: Vec<(i64, i64, i64)>,
    /// DBMS_XPLAN's report: ALLSTATS LAST with statistics, with the bind
    /// values the plan was made for.
    pub xplan: Result<String, String>,
}

impl CursorPlan {
    /// The plan has what an execution actually did.
    pub fn measured(&self) -> bool {
        pythia_ui::plan::has_actuals(&self.rows)
    }
}

/// The plan of child `child` of `sql_id`, with statistics if a finished
/// execution collected them.
pub fn cursor_plan(conn: &Connection, sql_id: &str, child: i64) -> Result<CursorPlan, DbError> {
    let (plan_hash, executions, users, text): (i64, i64, i64, Option<String>) = conn.query_row_as(
        "select plan_hash_value, executions, users_executing, sql_fulltext \
               from v$sql where sql_id = :1 and child_number = :2",
        &[&sql_id, &child],
    )?;
    let rows = match pythia_db::cursor_plan(conn, sql_id, child)? {
        MetaResult::Table { columns, rows } => pythia_ui::plan::cursor_rows(&columns, &rows),
        _ => Vec::new(),
    };
    let other_plans = conn
        .query_as::<(i64, i64, i64)>(
            "select child_number, plan_hash_value, executions from v$sql \
              where sql_id = :1 and plan_hash_value <> :2 and plan_hash_value <> 0 \
              order by executions desc",
            &[&sql_id, &plan_hash],
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let format = if pythia_ui::plan::has_actuals(&rows) {
        "ALLSTATS LAST +PEEKED_BINDS"
    } else {
        "TYPICAL +PEEKED_BINDS"
    };
    let xplan = conn
        .query_as::<Option<String>>(
            "select plan_table_output from table(dbms_xplan.display_cursor(:1, :2, :3))",
            &[&sql_id, &child, &format],
        )
        .and_then(|lines| lines.collect::<Result<Vec<_>, _>>())
        .map(|lines| {
            lines
                .into_iter()
                .map(Option::unwrap_or_default)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .map_err(|e| DbError::from(e).text());
    Ok(CursorPlan {
        sql_id: sql_id.to_owned(),
        child,
        rows,
        text: text.unwrap_or_default(),
        executions,
        running: users > 0,
        other_plans,
        xplan,
    })
}

/// A lookup's error, with what to grant when the views can't be read.
pub fn explain_error(e: &DbError) -> String {
    match e.code {
        Some(942 | 1031) => format!(
            "{}\nReading the session's SQL needs SELECT_CATALOG_ROLE (or SELECT on V$OPEN_CURSOR, \
             V$SQL and V$SQL_PLAN_STATISTICS_ALL).",
            e.text()
        ),
        _ => e.text(),
    }
}
