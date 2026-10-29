//! The SQL panel's lookups against a real database: the statements the
//! stopped session ran, by line, and their plans with actual rows. Needs
//! the same environment as `debug_session.rs`.

mod common;

use std::sync::{Mutex, mpsc};
use std::thread;

use common::{Harness, connect, host};
use pythia_debugger::sql::{self, SessionSql, TargetSession};
use pythia_debugger::target::{self, DebugOutcome};
use pythia_debugger::{Breakpoint, DebugCommand, Unit, UnitType};

static LOCK: Mutex<()> = Mutex::new(());

const TABLE: &str = "create table dbgq_t as \
    select level n, rpad('x', 20, 'x') pad from dual connect by level <= 2000";

// The count's predicate is estimated at 1 row and matches all 2000.
const PROC: &str = "create or replace procedure dbgq_proc(p_limit number) is
  l_count number;
  l_sum   number := 0;
begin
  select count(*) into l_count from dbgq_t where upper(to_char(n)) = to_char(n); -- @count
  for r in (select n from dbgq_t where n <= p_limit) loop -- @loop
    l_sum := l_sum + r.n; -- @inside
  end loop;
  execute immediate 'select count(*) from dbgq_t where n > :1' into l_count using p_limit; -- @dynamic
  l_sum := l_sum + l_count; -- @after
end;";

const RUN: &str = "declare
  x number;
begin
  select count(*) into x from dbgq_t where n < 10;
  dbgq_proc(3);
end;";

fn unit() -> Unit {
    Unit::new("PYTHIA", "DBGQ_PROC", UnitType::Procedure)
}

/// The line of a `-- @marker` in the procedure.
fn line(marker: &str) -> u32 {
    PROC.lines()
        .position(|l| l.ends_with(&format!("-- @{marker}")))
        .unwrap_or_else(|| panic!("no marker {marker}")) as u32
        + 1
}

fn at<'a>(list: &'a [SessionSql], unit: Option<&Unit>, line: u32) -> &'a SessionSql {
    list.iter()
        .find(|s| s.unit.as_ref() == unit && s.line == line)
        .unwrap_or_else(|| panic!("no statement at {unit:?} line {line} in {list:#?}"))
}

#[test]
fn statements_by_line_with_actual_rows() {
    let (Some(dba), Some(conn)) = (connect("PYTHIA_TEST_DB_DBA"), connect("PYTHIA_TEST_DB")) else {
        eprintln!("PYTHIA_TEST_DB_DBA / PYTHIA_TEST_DB not set, skipping");
        return;
    };
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for stmt in target::setup_statements("pythia", &host()) {
        dba.execute(&stmt, &[])
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    let _ = conn.execute("drop table dbgq_t purge", &[]);
    conn.execute(TABLE, &[]).unwrap();
    conn.execute(
        "begin dbms_stats.gather_table_stats(user, 'DBGQ_T'); end;",
        &[],
    )
    .unwrap();
    conn.execute(PROC, &[]).unwrap();
    target::compile_debug(&conn, &unit()).unwrap();

    let h = Harness::new();
    for marker in ["inside", "after"] {
        h.send(DebugCommand::SetBreakpoint(Breakpoint {
            unit: unit(),
            line: line(marker),
        }));
    }
    let port = h.debugger.port();
    let (tx, rx) = mpsc::channel();
    let host = host();
    let target = thread::spawn(move || {
        let conn = connect("PYTHIA_TEST_DB").unwrap();
        let session = target::session_task()(&conn).unwrap();
        tx.send(*session.downcast::<TargetSession>().unwrap())
            .unwrap();
        let outcome = target::debug_task(host, port, RUN.into(), true)(&conn).unwrap();
        let level: String = conn
            .query_row_as(
                "select value from v$parameter where name = 'statistics_level'",
                &[],
            )
            .unwrap();
        (*outcome.downcast::<DebugOutcome>().unwrap(), level)
    });
    let session = rx.recv().unwrap();

    // Stopped inside the loop: the count ran, the loop's cursor is open.
    let (_, stopped, stopped_line, _, _) = h.stop();
    assert_eq!((stopped, stopped_line), (unit(), Some(line("inside"))));
    let list = sql::session_sql(&conn, &session).unwrap();
    let count = at(&list, Some(&unit()), line("count"));
    assert!(
        count.text.starts_with("SELECT COUNT(*) FROM DBGQ_T"),
        "{count:?}"
    );
    assert!(!count.running);
    let looping = at(&list, Some(&unit()), line("loop"));
    assert!(looping.running, "{looping:?}");
    // The block's statement: no unit, the block's own line.
    let block = at(&list, None, 4);
    assert!(block.text.contains("N < 10"), "{block:?}");
    // The running one first.
    assert_eq!(list[0].sql_id, looping.sql_id);

    let plan = sql::cursor_plan(&conn, &count.sql_id, count.child).unwrap();
    assert!(plan.measured(), "{plan:#?}");
    let worst = pythia_ui::plan::worst_misestimate(&plan.rows).expect("a misestimate");
    assert!(worst.operation.starts_with("TABLE ACCESS"), "{worst:?}");
    assert_eq!(worst.actual_rows, Some(2000.0));
    assert!(worst.misestimate().unwrap() >= 100.0, "{worst:?}");
    assert!(
        plan.xplan.as_ref().unwrap().contains("A-Rows"),
        "{:?}",
        plan.xplan
    );
    assert!(plan.text.contains("UPPER(TO_CHAR(N))"), "{}", plan.text);

    // Its first execution is still running: no actual rows yet.
    let plan = sql::cursor_plan(&conn, &looping.sql_id, looping.child).unwrap();
    assert!(plan.running);
    assert!(!plan.measured(), "{plan:#?}");
    assert!(plan.rows.iter().all(|r| r.rows.is_some() || r.id == 0));

    // Past the loop and the dynamic statement.
    h.send(DebugCommand::Resume);
    let mut stop = h.stop();
    while stop.2 == Some(line("inside")) {
        h.send(DebugCommand::Resume);
        stop = h.stop();
    }
    assert_eq!((stop.1, stop.2), (unit(), Some(line("after"))));
    let list = sql::session_sql(&conn, &session).unwrap();
    let looping = at(&list, Some(&unit()), line("loop"));
    assert!(!looping.running);
    let plan = sql::cursor_plan(&conn, &looping.sql_id, looping.child).unwrap();
    assert!(plan.measured(), "{plan:#?}");
    assert_eq!(plan.rows[0].actual_rows, Some(3.0));
    // Dynamic SQL too, as written.
    let dynamic = at(&list, Some(&unit()), line("dynamic"));
    assert_eq!(dynamic.text, "select count(*) from dbgq_t where n > :1");

    h.send(DebugCommand::Resume);
    h.until_terminated();
    let (outcome, level) = target.join().unwrap();
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    // The session's setting is back.
    assert_eq!(level, "TYPICAL");
    conn.execute("drop procedure dbgq_proc", &[]).unwrap();
    conn.execute("drop table dbgq_t purge", &[]).unwrap();
}
