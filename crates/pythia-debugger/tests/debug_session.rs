//! End-to-end debugging against a real database.
//!
//! Skipped unless both are set:
//! `PYTHIA_TEST_DB_DBA=system/oracle@localhost:1521/FREEPDB1`
//! `PYTHIA_TEST_DB=pythia/pythia@localhost:1521/FREEPDB1`
//! The database must reach this machine as `PYTHIA_DEBUG_HOST`
//! (default `host.containers.internal`, right for a local container).

mod common;

use std::thread;

use common::{Harness, connect, host, run_target, value, watch};
use pythia_debugger::{
    Breakpoint, BreakpointOptions, DebugCommand, DebugEvent, StopReason, Unit, UnitType,
};
use pythia_debugger::{setup, target};

const DEMO: &str = "create or replace procedure dbg_demo(p_n in number) is
  type t_arr is table of number; type t_rec is record (a number := 7, b varchar2(10) := 'bee');
  l_arr   t_arr := t_arr(10, 20, 30); l_rec t_rec;
  l_total number := 0;
  l_name  varchar2(30) := 'start';
  l_flag  boolean := true;
  l_date  date := date '2024-02-29';
begin
  for i in 1 .. p_n loop
    l_total := l_total + i;
    l_name := 'step ' || i;
  end loop;
  dbg_helper(l_total);
  l_name := 'done';
end;";

const HELPER: &str = "create or replace procedure dbg_helper(p_v in out number) is
  l_x number := p_v * 2;
begin
  p_v := l_x + 1;
end;";

const PKG: &str = "create or replace package dbg_pkg as
  procedure p;
end;";

const PKG_BODY: &str = "create or replace package body dbg_pkg as
  g_count number := 0;
  procedure p is
  begin
    g_count := g_count + 1;
  end;
end;";

const EXC: &str = "create or replace procedure dbg_exc is
  l_x number := 0;
begin
  begin
    l_x := 1 / l_x;
  exception
    when zero_divide then
      l_x := -1;
  end;
  dbms_output.put_line('handled ' || l_x);
end;";

#[test]
fn debug_procedures_and_packages() {
    let (Some(dba), Some(conn)) = (connect("PYTHIA_TEST_DB_DBA"), connect("PYTHIA_TEST_DB")) else {
        eprintln!("PYTHIA_TEST_DB_DBA / PYTHIA_TEST_DB not set, skipping");
        return;
    };
    // Setup: privileges and the JDWP ACL for the test user.
    for stmt in target::setup_statements("pythia", &host()) {
        dba.execute(&stmt, &[])
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    let facts = setup::facts(&conn).unwrap();
    assert!(facts.debug_connect && facts.debug_any, "{facts:?}");
    assert_eq!(facts.user, "PYTHIA");

    for ddl in [HELPER, DEMO, PKG, PKG_BODY, EXC] {
        conn.execute(ddl, &[]).unwrap();
    }
    let demo = Unit::new("PYTHIA", "DBG_DEMO", UnitType::Procedure);
    let helper = Unit::new("PYTHIA", "DBG_HELPER", UnitType::Procedure);
    let body = Unit::new("PYTHIA", "DBG_PKG", UnitType::PackageBody);
    let exc = Unit::new("PYTHIA", "DBG_EXC", UnitType::Procedure);
    for unit in [
        &demo,
        &helper,
        &Unit::new("PYTHIA", "DBG_PKG", UnitType::Package),
        &body,
        &exc,
    ] {
        target::compile_debug(&conn, unit).unwrap();
    }
    assert!(target::has_debug_info(&conn, &demo).unwrap());

    // --- procedure: deferred breakpoint, variables, stepping ---------------
    let h = Harness::new();
    // Set before the target connects: bound when the unit is loaded.
    h.debugger.send(DebugCommand::SetBreakpoint(Breakpoint {
        unit: demo.clone(),
        line: 10,
    }));
    let target = run_target(h.debugger.port(), "begin dbg_demo(3); end;");

    let (reason, unit, line, vars, depth) = h.stop();
    assert_eq!(
        (reason, &unit, line),
        (StopReason::Breakpoint, &demo, Some(10))
    );
    assert_eq!(depth, 2, "dbg_demo called from the anonymous block");
    assert_eq!(value(&vars, "P_N"), Some("3"));
    assert_eq!(value(&vars, "L_TOTAL"), Some("0"));
    assert_eq!(value(&vars, "L_NAME"), Some("start"));
    assert_eq!(value(&vars, "L_FLAG"), Some("true"));
    assert!(
        value(&vars, "L_DATE").is_some_and(|d| d.contains("24")),
        "{vars:?}"
    );
    assert_eq!(value(&vars, "I"), Some("1"));
    let arr = vars
        .iter()
        .find(|v| v.name == "L_ARR")
        .expect("collection variable");
    eprintln!("collection: {arr:?}");
    assert_eq!(arr.type_name, "T_ARR");
    let elements: Vec<Option<&str>> = arr.children.iter().map(|c| c.value.as_deref()).collect();
    assert_eq!(elements, [Some("10"), Some("20"), Some("30")], "{arr:?}");
    let rec = vars
        .iter()
        .find(|v| v.name == "L_REC")
        .expect("record variable");
    eprintln!("record: {rec:?}");
    assert_eq!(value(&rec.children, "A"), Some("7"), "{rec:?}");
    assert_eq!(value(&rec.children, "B"), Some("bee"), "{rec:?}");

    // Step over: the line advances within the loop.
    h.debugger.send(DebugCommand::StepOver);
    let (reason, _, line, vars, _) = h.stop();
    assert_eq!((reason, line), (StopReason::Step, Some(11)));
    assert_eq!(value(&vars, "L_TOTAL"), Some("1"));
    h.debugger.send(DebugCommand::StepOver);
    let (_, _, line, vars, _) = h.stop();
    assert_ne!(line, Some(11), "stepping moved on");
    assert_eq!(value(&vars, "L_NAME"), Some("step 1"));

    // Move the breakpoint to the helper call and step into it.
    h.debugger.send(DebugCommand::ClearBreakpoint(Breakpoint {
        unit: demo.clone(),
        line: 10,
    }));
    h.debugger.send(DebugCommand::SetBreakpoint(Breakpoint {
        unit: demo.clone(),
        line: 13,
    }));
    h.debugger.send(DebugCommand::Resume);
    let (reason, unit, line, vars, _) = h.stop();
    assert_eq!(
        (reason, &unit, line),
        (StopReason::Breakpoint, &demo, Some(13))
    );
    assert_eq!(value(&vars, "L_TOTAL"), Some("6"));
    h.debugger.send(DebugCommand::StepInto);
    let (_, unit, _, vars, depth) = h.stop();
    assert_eq!(unit, helper, "stepped into dbg_helper");
    assert_eq!(depth, 3);
    assert_eq!(value(&vars, "P_V"), Some("6"));
    h.debugger.send(DebugCommand::StepOut);
    let (_, unit, line, _, _) = h.stop();
    assert_eq!(unit, demo, "stepped out to dbg_demo");
    assert!(line.is_some_and(|l| l >= 13), "{line:?}");
    h.debugger.send(DebugCommand::Resume);
    h.until_terminated();
    target.join().unwrap().expect("target ran to completion");

    // --- package body: class naming ------------------------------------------
    let h = Harness::new();
    h.debugger.send(DebugCommand::SetBreakpoint(Breakpoint {
        unit: body.clone(),
        line: 5,
    }));
    h.debugger.send(DebugCommand::SetWatches(vec![
        "g_count".into(),
        "dbg_pkg.g_count".into(),
        "no_such_thing".into(),
    ]));
    let target = run_target(h.debugger.port(), "begin dbg_pkg.p; dbg_pkg.p; end;");
    let (stop, _) = h.stop_full();
    assert_eq!((&stop.unit, stop.line), (&body, Some(5)));
    eprintln!("package vars: {:?}", stop.variables);
    eprintln!("package watches: {:?}", stop.watches);
    assert_eq!(watch(&stop.watches, "g_count"), Ok(Some("0".into())));
    assert_eq!(
        watch(&stop.watches, "dbg_pkg.g_count"),
        Ok(Some("0".into()))
    );
    assert!(watch(&stop.watches, "no_such_thing").is_err());
    h.debugger.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    assert_eq!(
        (&stop.unit, stop.line),
        (&body, Some(5)),
        "second call stops again"
    );
    assert_eq!(watch(&stop.watches, "g_count"), Ok(Some("1".into())));
    // Terminating aborts the target call.
    h.debugger.send(DebugCommand::Terminate);
    h.until_terminated();
    let result = target.join().unwrap();
    assert!(result.is_err(), "terminated call fails in the target");

    // --- conditions, hit counts and watches ----------------------------------
    let h = Harness::new();
    let at10 = Breakpoint {
        unit: demo.clone(),
        line: 10,
    };
    let at11 = Breakpoint {
        unit: demo.clone(),
        line: 11,
    };
    h.debugger.send(DebugCommand::SetBreakpoint(at10.clone()));
    h.debugger.send(DebugCommand::SetBreakpointOptions(
        at10.clone(),
        BreakpointOptions {
            condition: Some("i = 2 and l_name is not null".into()),
            hit_count: None,
        },
    ));
    h.debugger.send(DebugCommand::SetWatches(vec![
        "l_rec.b".into(),
        "L_ARR(2)".into(),
        "l_total".into(),
    ]));
    let target = run_target(h.debugger.port(), "begin dbg_demo(4); end;");
    let (stop, _) = h.stop_full();
    assert_eq!((stop.reason, stop.line), (StopReason::Breakpoint, Some(10)));
    assert_eq!(
        value(&stop.variables, "I"),
        Some("2"),
        "condition skipped i = 1"
    );
    assert_eq!(watch(&stop.watches, "l_rec.b"), Ok(Some("bee".into())));
    assert_eq!(watch(&stop.watches, "L_ARR(2)"), Ok(Some("20".into())));
    assert_eq!(watch(&stop.watches, "l_total"), Ok(Some("1".into())));
    // Watches added while stopped are evaluated right away.
    h.debugger
        .send(DebugCommand::SetWatches(vec!["l_name".into()]));
    let watches = h.watches();
    assert_eq!(watch(&watches, "l_name"), Ok(Some("step 1".into())));
    // Hit count: line 11 stops from its second hit on (i = 2 is the first).
    h.debugger.send(DebugCommand::ClearBreakpoint(at10));
    h.debugger.send(DebugCommand::SetBreakpoint(at11.clone()));
    h.debugger.send(DebugCommand::SetBreakpointOptions(
        at11,
        BreakpointOptions {
            condition: None,
            hit_count: Some(2),
        },
    ));
    h.debugger.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    assert_eq!(stop.line, Some(11));
    assert_eq!(
        value(&stop.variables, "I"),
        Some("3"),
        "{:?}",
        stop.variables
    );
    h.debugger.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    assert_eq!(value(&stop.variables, "I"), Some("4"), "hit 3 stops too");
    h.debugger.send(DebugCommand::Resume);
    h.until_terminated();
    target.join().unwrap().expect("target ran to completion");

    // --- break on exception ---------------------------------------------------
    let h = Harness::new();
    h.debugger.send(DebugCommand::BreakOnException(true));
    let target = run_target(h.debugger.port(), "begin dbg_exc; end;");
    let (stop, _) = h.stop_full();
    eprintln!("exception stop: {:?} {:?}", stop.exception, stop.line);
    assert_eq!((stop.reason, &stop.unit), (StopReason::Exception, &exc));
    assert_eq!(stop.line, Some(5), "stopped where it was raised");
    assert_eq!(stop.exception.as_deref(), Some("ORA-01476 (zero_divide)"));
    h.debugger.send(DebugCommand::Resume);
    h.until_terminated();
    target
        .join()
        .unwrap()
        .expect("handled exception doesn't fail the call");

    // --- DBMS_OUTPUT of a debugged block -------------------------------------
    let h = Harness::new();
    let host = host();
    let port = h.debugger.port();
    let task = thread::spawn(move || {
        let conn = connect("PYTHIA_TEST_DB").unwrap();
        conn.execute("begin dbms_output.enable(null); end;", &[])
            .unwrap();
        let task = target::debug_task(host, port, "begin dbg_exc; end;".into(), false);
        task(&conn)
    });
    h.until_terminated();
    let outcome = task
        .join()
        .unwrap()
        .unwrap()
        .downcast::<target::DebugOutcome>()
        .unwrap();
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.output, ["handled -1"]);

    // --- stop on entry: a procedure, once -------------------------------------
    let h = Harness::new();
    h.debugger.send(DebugCommand::BreakOnEntry {
        unit: helper.clone(),
        subprogram: "DBG_HELPER".into(),
    });
    let target = run_target(h.debugger.port(), "begin dbg_demo(2); dbg_demo(1); end;");
    let (stop, depth) = h.stop_full();
    eprintln!("entry: {stop:?}");
    assert_eq!((stop.reason, &stop.unit), (StopReason::Entry, &helper));
    assert_eq!(depth, 3, "dbg_helper called from dbg_demo");
    assert_eq!(stop.line, Some(1), "the procedure's header");
    assert_eq!(value(&stop.variables, "P_V"), Some("3"));
    // The second call doesn't stop.
    h.debugger.send(DebugCommand::Resume);
    assert!(h.next_stop().is_none(), "stops once");
    target.join().unwrap().expect("target ran to completion");

    // --- stop on entry: no such subprogram -------------------------------------
    let h = Harness::new();
    h.debugger.send(DebugCommand::BreakOnEntry {
        unit: body.clone(),
        subprogram: "NOT_THERE".into(),
    });
    let target = run_target(h.debugger.port(), "begin dbg_pkg.p; end;");
    let error = loop {
        match h.next() {
            DebugEvent::Error(e) => break e,
            DebugEvent::Terminated => panic!("no error about the missing subprogram"),
            _ => {}
        }
    };
    assert!(error.contains("not_there"), "{error}");
    assert!(h.next_stop().is_none(), "runs through");
    target.join().unwrap().expect("target ran to completion");

    for ddl in [
        "drop procedure dbg_demo",
        "drop procedure dbg_helper",
        "drop procedure dbg_exc",
        "drop package dbg_pkg",
    ] {
        conn.execute(ddl, &[]).unwrap();
    }
}
