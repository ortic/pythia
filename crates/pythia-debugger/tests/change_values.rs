//! Changing values while stopped and running to a line, against a real
//! database. Needs the same environment as `debug_session.rs`.

mod common;

use std::sync::Mutex;
use std::thread;

use common::{Harness, connect, host, val};
use pythia_debugger::target::{self, DebugOutcome};
use pythia_debugger::{Breakpoint, DebugCommand, DebugEvent, StopReason, Unit, UnitType, Variable};

/// The tests share the package; one at a time.
static LOCK: Mutex<()> = Mutex::new(());

const SPEC: &str = "create or replace package dbgv_pkg is
  g_num number := 7;
  procedure run(p_n number, p_io in out number);
end;";

const BODY: &str = "create or replace package body dbgv_pkg is
  procedure run(p_n number, p_io in out number) is
    l_total number := 0;
    l_name  varchar2(5) := 'start';
    l_np    number(5,2) := 1.25;
    l_int   pls_integer := 5;
    l_date  date := date '2024-02-29';
    l_flag  boolean := true;
    l_raw   raw(4) := hextoraw('0AFF');
    l_bd    binary_double := 1;
    l_nn    number not null := 1;
    type t_rec is record (a number, b varchar2(10));
    l_rec   t_rec;
    type t_map is table of number index by varchar2(10);
    l_map   t_map;
    type t_list is table of varchar2(10);
    l_list  t_list := t_list('a', 'b');
  begin
    l_rec.a := 1;
    l_map('Key') := 10;
    l_total := l_total + p_n; -- @set
    for i in 1 .. 3 loop
      l_int := l_int + i; -- @loop
    end loop;
    dbgv_later(l_int);
    dbms_output.put_line(l_total || '|' || nvl(l_name, '(null)') || '|' || l_np || '|' || l_int
      || '|' || to_char(l_date, 'yyyy-mm-dd') || '|' || case when l_flag then 'T' else 'F' end
      || '|' || rawtohex(l_raw) || '|' || to_char(l_bd) || '|' || l_nn || '|' || l_rec.a
      || '|' || l_map('Key') || '|' || l_list(2) || '|' || g_num || '|' || p_io);
  end;
end;";

const LATER: &str = "create or replace procedure dbgv_later(p_v number) is
  l_x number;
begin
  l_x := p_v * 2;
end;";

const RUN: &str = "declare x number := 4; begin dbgv_pkg.run(3, x); end;";

fn body() -> Unit {
    Unit::new("PYTHIA", "DBGV_PKG", UnitType::PackageBody)
}

/// The line of a `-- @marker` in the package body.
fn at(marker: &str) -> Breakpoint {
    let line = BODY
        .lines()
        .position(|l| l.ends_with(&format!("-- @{marker}")))
        .unwrap_or_else(|| panic!("no marker {marker}"));
    Breakpoint {
        unit: body(),
        line: line as u32 + 1,
    }
}

fn setup() -> Option<std::sync::MutexGuard<'static, ()>> {
    let (Some(dba), Some(conn)) = (connect("PYTHIA_TEST_DB_DBA"), connect("PYTHIA_TEST_DB")) else {
        eprintln!("PYTHIA_TEST_DB_DBA / PYTHIA_TEST_DB not set, skipping");
        return None;
    };
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for stmt in target::setup_statements("pythia", &host()) {
        dba.execute(&stmt, &[])
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    for ddl in [LATER, SPEC, BODY] {
        conn.execute(ddl, &[]).unwrap();
    }
    for unit in [
        Unit::new("PYTHIA", "DBGV_LATER", UnitType::Procedure),
        Unit::new("PYTHIA", "DBGV_PKG", UnitType::Package),
        body(),
    ] {
        target::compile_debug(&conn, &unit).unwrap();
    }
    Some(lock)
}

/// Runs [`RUN`] under the debugger with DBMS_OUTPUT enabled, after `nls`.
fn run(port: u16, nls: Option<&'static str>) -> thread::JoinHandle<DebugOutcome> {
    let host = host();
    thread::spawn(move || {
        let conn = connect("PYTHIA_TEST_DB").unwrap();
        if let Some(nls) = nls {
            conn.execute(nls, &[]).unwrap();
        }
        conn.execute("begin dbms_output.enable(null); end;", &[])
            .unwrap();
        let task = target::debug_task(host, port, RUN.into(), false);
        *task(&conn).unwrap().downcast::<DebugOutcome>().unwrap()
    })
}

/// Changes a value: the frame's variables read again, or why not.
fn set(h: &Harness, expression: &str, value: Option<&str>) -> Result<Vec<Variable>, String> {
    h.send(DebugCommand::SetValue {
        expression: expression.into(),
        value: value.map(Into::into),
    });
    loop {
        match h.next() {
            DebugEvent::ValueSet {
                error: Some(error), ..
            } => return Err(error),
            DebugEvent::FrameSelected { variables, .. } => return Ok(variables),
            DebugEvent::Terminated => panic!("the session ended setting {expression}"),
            DebugEvent::Error(e) => panic!("debugger error: {e}"),
            _ => {}
        }
    }
}

#[test]
fn change_values_while_stopped() {
    let Some(_lock) = setup() else {
        return;
    };
    let h = Harness::new();
    h.send(DebugCommand::SetBreakpoint(at("set")));
    let target = run(h.debugger.port(), None);
    let (stop, _) = h.stop_full();
    assert_eq!(
        (stop.reason, stop.line),
        (StopReason::Breakpoint, Some(at("set").line))
    );

    let vars = set(&h, "l_total", Some("100")).unwrap();
    assert_eq!(val(&vars, "L_TOTAL"), Some("100"));
    let vars = set(&h, "L_Name", None).unwrap();
    assert_eq!(val(&vars, "L_NAME"), None, "NULL");
    let vars = set(&h, "p_io", Some("9")).unwrap();
    assert_eq!(val(&vars, "P_IO"), Some("9"), "IN OUT parameter");
    let vars = set(&h, "l_np", Some("12.5")).unwrap();
    assert_eq!(val(&vars, "L_NP"), Some("12.5"));
    let vars = set(&h, "l_rec.a", Some("5")).unwrap();
    assert_eq!(val(&vars, "L_REC.A"), Some("5"), "record field");
    let vars = set(&h, "l_map('Key')", Some("11")).unwrap();
    assert_eq!(val(&vars, "L_MAP.('Key')"), Some("11"), "element by key");
    let vars = set(&h, "l_list(2)", Some("zz")).unwrap();
    assert_eq!(val(&vars, "L_LIST.(2)"), Some("zz"), "element by position");
    set(&h, "l_flag", Some("FALSE")).unwrap();
    set(&h, "l_raw", Some("0102")).unwrap();
    set(&h, "l_bd", Some("1500")).unwrap();
    set(&h, "g_num", Some("8")).expect("a global of the package");

    // Refused, and the session goes on: some of these would end it if
    // the agent got them.
    let refused = |expression: &str, value: Option<&str>, reason: &str| {
        let error = set(&h, expression, value).expect_err(expression);
        assert!(error.contains(reason), "{expression}: {error}");
    };
    refused("p_n", Some("9"), "IN parameters are read-only");
    refused("l_np", Some("1234.5"), "too large for NUMBER(5,2)");
    refused("l_int", Some("abc"), "isn't a number");
    refused("l_int", Some("3000000000"), "out of range");
    refused("l_raw", Some("ZZ"), "isn't hexadecimal");
    refused("l_nn", None, "NOT NULL");
    refused("l_name", Some("much too long"), "too long for VARCHAR2(5)");
    refused(
        "l_date",
        Some("garbage"),
        "not a date in the session's format, e.g.",
    );
    refused("l_rec", Some("1"), "change its fields");
    refused("l_nothing", Some("1"), "unknown");

    h.send(DebugCommand::Resume);
    assert!(h.next_stop().is_none());
    let outcome = target.join().unwrap();
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(
        outcome.output,
        ["103|(null)|12.5|11|2024-02-29|F|0102|1.5E+003|1|5|11|zz|8|9"],
        "the code went on with the new values, and the date is as it was"
    );
}

/// A session with a decimal comma: numbers are written its way.
#[test]
fn change_values_with_a_decimal_comma() {
    let Some(_lock) = setup() else {
        return;
    };
    let h = Harness::new();
    h.send(DebugCommand::SetBreakpoint(at("set")));
    let target = run(
        h.debugger.port(),
        Some("alter session set nls_numeric_characters = ',.'"),
    );
    h.stop();
    let vars = set(&h, "l_np", Some("12.5")).unwrap();
    assert_eq!(val(&vars, "L_NP"), Some("12,5"));
    let vars = set(&h, "l_total", Some("2,5")).unwrap();
    assert_eq!(val(&vars, "L_TOTAL"), Some("2,5"));
    h.send(DebugCommand::Resume);
    h.until_terminated();
    let outcome = target.join().unwrap();
    assert!(
        outcome.output[0].starts_with("5,5|start|12,5|"),
        "{:?}",
        outcome.output
    );
}

#[test]
fn run_to_cursor() {
    let Some(_lock) = setup() else {
        return;
    };
    let later = Unit::new("PYTHIA", "DBGV_LATER", UnitType::Procedure);
    let h = Harness::new();
    h.send(DebugCommand::SetBreakpoint(at("set")));
    let target = run(h.debugger.port(), None);
    h.stop();

    h.send(DebugCommand::RunTo(at("loop")));
    let (stop, _) = h.stop_full();
    assert_eq!(
        (stop.reason, stop.line),
        (StopReason::Cursor, Some(at("loop").line))
    );
    assert_eq!(val(&stop.variables, "I"), Some("1"));

    // From the line itself: the next time it's reached.
    h.send(DebugCommand::RunTo(at("loop")));
    let (stop, _) = h.stop_full();
    assert_eq!(stop.reason, StopReason::Cursor);
    assert_eq!(val(&stop.variables, "I"), Some("2"));

    // A breakpoint on the way stops first and ends the run to the line.
    h.send(DebugCommand::SetBreakpoint(at("loop")));
    h.send(DebugCommand::RunTo(Breakpoint {
        unit: later.clone(),
        line: 4,
    }));
    let (stop, _) = h.stop_full();
    assert_eq!(stop.reason, StopReason::Breakpoint);
    assert_eq!(val(&stop.variables, "I"), Some("3"));
    h.send(DebugCommand::ClearBreakpoint(at("loop")));

    // A unit not loaded yet: it stops there once it is.
    h.send(DebugCommand::RunTo(Breakpoint {
        unit: later.clone(),
        line: 4,
    }));
    let (stop, _) = h.stop_full();
    assert_eq!(
        (stop.reason, &stop.unit, stop.line),
        (StopReason::Cursor, &later, Some(4))
    );
    assert_eq!(val(&stop.variables, "P_V"), Some("11"));

    h.send(DebugCommand::Resume);
    assert!(h.next_stop().is_none(), "stops once");
    let outcome = target.join().unwrap();
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
}

/// The agent sets no breakpoints in anonymous blocks: running to a line of
/// the block steps there.
#[test]
fn run_to_cursor_in_a_block() {
    let Some(_lock) = setup() else {
        return;
    };
    const BLOCK: &str = "declare
  x number := 4;
begin
  dbgv_pkg.run(3, x);
  x := x + 1;
  x := x + 2;
  dbms_output.put_line(x);
end;";
    let h = Harness::new();
    h.send(DebugCommand::SetBreakpoint(at("set")));
    let target = common::run_target(h.debugger.port(), BLOCK);
    let (stop, _) = h.stop_full();
    let block = stop.stack.last().unwrap().location.unit.clone();
    assert_eq!(block.unit_type, UnitType::Block);

    // From inside the package it called.
    h.send(DebugCommand::RunTo(Breakpoint {
        unit: block.clone(),
        line: 6,
    }));
    let (stop, depth) = h.stop_full();
    assert_eq!(
        (stop.reason, &stop.unit, stop.line),
        (StopReason::Cursor, &block, Some(6))
    );
    assert_eq!(depth, 1);
    assert_eq!(val(&stop.variables, "X"), Some("5"), "line 5 ran");

    // No code there: it says so and stays where it is.
    h.send(DebugCommand::RunTo(Breakpoint {
        unit: block.clone(),
        line: 99,
    }));
    let error = loop {
        if let DebugEvent::Error(e) = h.next() {
            break e;
        }
    };
    assert!(error.contains("no code at or after it"), "{error}");
    let stop = loop {
        if let DebugEvent::Suspended { location, .. } = h.next() {
            break location;
        }
    };
    assert_eq!(stop.line, Some(6), "the same stop, again");

    h.send(DebugCommand::Resume);
    assert!(h.next_stop().is_none());
    target.join().unwrap().expect("the block ran to its end");
}
