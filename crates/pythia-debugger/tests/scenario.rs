//! A larger debugging scenario against a real database: order processing
//! across packages, records, `%ROWTYPE`, collections, an object type, a
//! trigger, recursion and exceptions (`tests/scenario/orders.sql`).
//!
//! Needs the same environment as `debug_session.rs`.

mod common;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use common::{Harness, Stop, connect, host, run_target, val, var, watch};
use pythia_db::oracle::Connection;
use pythia_debugger::target::{self, DebugOutcome};
use pythia_debugger::{
    Breakpoint, BreakpointOptions, DebugCommand, DebugEvent, StopReason, Unit, UnitType,
};

const SCRIPT: &str = include_str!("scenario/orders.sql");
/// The tests share the scenario's tables; one at a time.
static LOCK: Mutex<()> = Mutex::new(());
const OWNER: &str = "PYTHIA";
const RUN: &str = "declare
  l_count pls_integer;
begin
  dbgx_orders_api.process_all(l_count);
  dbms_output.put_line('processed ' || l_count);
  commit;
end;";

/// The scenario's PL/SQL units with their source, and `-- @name` markers.
struct Scenario {
    sources: BTreeMap<Unit, String>,
    markers: BTreeMap<String, (Unit, u32)>,
}

impl Scenario {
    fn units() -> Vec<Unit> {
        let u = |name: &str, t| Unit::new(OWNER, name, t);
        vec![
            u("DBGX_MONEY", UnitType::Type),
            u("DBGX_MONEY", UnitType::TypeBody),
            u("DBGX_TYPES", UnitType::Package),
            u("DBGX_PRICING", UnitType::Package),
            u("DBGX_PRICING", UnitType::PackageBody),
            u("DBGX_ORDERS_API", UnitType::Package),
            u("DBGX_ORDERS_API", UnitType::PackageBody),
            u("DBGX_ORDERS_BU", UnitType::Trigger),
        ]
    }

    /// Drops what an earlier run left, creates everything and compiles the
    /// units with debug information.
    fn install(dba: &Connection, conn: &Connection) -> Self {
        for stmt in target::setup_statements(OWNER, &host()) {
            dba.execute(&stmt, &[])
                .unwrap_or_else(|e| panic!("{stmt}: {e}"));
        }
        for drop in [
            "drop trigger dbgx_orders_bu",
            "drop package dbgx_orders_api",
            "drop package dbgx_pricing",
            "drop package dbgx_types",
            "drop type dbgx_money force",
            "drop type dbgx_codes force",
            "drop table dbgx_audit purge",
            "drop table dbgx_order_lines purge",
            "drop table dbgx_orders purge",
            "drop table dbgx_customers purge",
        ] {
            let _ = conn.execute(drop, &[]);
        }
        let mut sources = BTreeMap::new();
        for stmt in pythia_plsql::split_statements(SCRIPT) {
            let sql = stmt.text(SCRIPT);
            conn.execute(sql, &[])
                .unwrap_or_else(|e| panic!("{e}\n{sql}"));
            if let Some(unit) = Self::units().into_iter().find(|u| Self::creates(sql, u)) {
                sources.insert(unit, sql.to_owned());
            }
        }
        for unit in Self::units() {
            target::compile_debug(conn, &unit).unwrap_or_else(|e| panic!("{unit}: {e}"));
            assert!(target::has_debug_info(conn, &unit).unwrap(), "{unit}");
        }
        let errors: i64 = conn
            .query_row_as(
                "select count(*) from user_errors where name like 'DBGX%'",
                &[],
            )
            .unwrap();
        assert_eq!(errors, 0, "scenario compiles cleanly");
        let mut markers = BTreeMap::new();
        for (unit, source) in &sources {
            for (i, line) in source.lines().enumerate() {
                if let Some((_, name)) = line.split_once("-- @") {
                    let name: String = name
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    let previous = markers.insert(name.clone(), (unit.clone(), i as u32 + 1));
                    assert!(previous.is_none(), "duplicate marker {name}");
                }
            }
        }
        Self { sources, markers }
    }

    /// Whether `sql` is the `create` statement of `unit`.
    fn creates(sql: &str, unit: &Unit) -> bool {
        let head = sql.lines().next().unwrap_or_default().to_uppercase();
        let kind = match unit.unit_type {
            UnitType::Type => "TYPE ",
            UnitType::TypeBody => "TYPE BODY ",
            UnitType::Package => "PACKAGE ",
            UnitType::PackageBody => "PACKAGE BODY ",
            UnitType::Trigger => "TRIGGER ",
            _ => return false,
        };
        head.strip_prefix(&format!("CREATE OR REPLACE {kind}{}", unit.name))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '(']))
    }

    fn at(&self, marker: &str) -> Breakpoint {
        let (unit, line) = self
            .markers
            .get(marker)
            .unwrap_or_else(|| panic!("no marker {marker}"));
        Breakpoint {
            unit: unit.clone(),
            line: *line,
        }
    }

    /// The source text of a unit's line.
    fn line(&self, unit: &Unit, line: Option<u32>) -> &str {
        let Some(line) = line else { return "?" };
        self.sources
            .get(unit)
            .and_then(|s| s.lines().nth(line as usize - 1))
            .map_or("?", str::trim)
    }

    fn describe(&self, stop: &Stop) -> String {
        let frames: Vec<String> = stop
            .stack
            .iter()
            .map(|f| format!("{}:{}", f.method, f.location.line.unwrap_or(0)))
            .collect();
        format!(
            "{:?} {} line {:?} [{}] `{}`",
            stop.reason,
            stop.unit,
            stop.line,
            frames.join(" < "),
            self.line(&stop.unit, stop.line)
        )
    }
}

fn setup() -> Option<(std::sync::MutexGuard<'static, ()>, Connection, Scenario)> {
    let (Some(dba), Some(conn)) = (connect("PYTHIA_TEST_DB_DBA"), connect("PYTHIA_TEST_DB")) else {
        eprintln!("PYTHIA_TEST_DB_DBA / PYTHIA_TEST_DB not set, skipping");
        return None;
    };
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scenario = Scenario::install(&dba, &conn);
    Some((lock, conn, scenario))
}

/// Runs `block` under the debugger with DBMS_OUTPUT enabled.
fn run_with_output(port: u16, block: &'static str) -> thread::JoinHandle<DebugOutcome> {
    let host = host();
    thread::spawn(move || {
        let conn = connect("PYTHIA_TEST_DB").unwrap();
        conn.execute("begin dbms_output.enable(null); end;", &[])
            .unwrap();
        let task = target::debug_task(host, port, block.into(), false);
        *task(&conn).unwrap().downcast::<DebugOutcome>().unwrap()
    })
}

fn methods(stop: &Stop) -> Vec<&str> {
    stop.stack.iter().map(|f| f.method.as_str()).collect()
}

/// Steps into everything from the start of `process_all` to the end: every
/// stop is on a line with code, a step always moves on, and the stack
/// stays consistent.
#[test]
fn step_into_everything() {
    let Some((_lock, _conn, s)) = setup() else {
        return;
    };
    let h = Harness::new();
    h.send(DebugCommand::SetBreakpoint(s.at("process_all")));
    let target = run_target(h.debugger.port(), RUN);
    let mut previous: Option<(usize, Unit, Option<u32>)> = None;
    let mut units = std::collections::BTreeSet::new();
    let mut in_block = 0;
    let mut steps = 0;
    while let Some(stop) = h.next_stop() {
        eprintln!("#{steps:03} {}", s.describe(&stop));
        steps += 1;
        assert!(steps < 500, "runaway stepping");
        assert!(stop.line.is_some(), "every stop has a line: {stop:?}");
        assert!(
            stop.stack.iter().all(|f| f.location.line.is_some()),
            "{stop:?}"
        );
        assert_eq!(stop.stack[0].location.unit, stop.unit);
        if stop.unit.unit_type == UnitType::Block {
            in_block += 1;
        } else {
            let text = s.line(&stop.unit, stop.line);
            assert!(
                !text.is_empty() && !text.starts_with("--"),
                "stopped on a line without code: {}",
                s.describe(&stop)
            );
            assert_eq!(
                methods(&stop).last(),
                Some(&"BLOCK"),
                "the anonymous block is the outermost frame"
            );
        }
        let here = (stop.stack.len(), stop.unit.clone(), stop.line);
        assert_ne!(
            previous.as_ref(),
            Some(&here),
            "a step stayed put: {}",
            s.describe(&stop)
        );
        previous = Some(here);
        units.insert(stop.unit.clone());
        h.send(DebugCommand::StepInto);
    }
    target.join().unwrap().expect("target ran to completion");
    for unit in Scenario::units() {
        if matches!(unit.unit_type, UnitType::Type | UnitType::Package) {
            continue;
        }
        assert!(units.contains(&unit), "stepped through {unit}");
    }
    assert!(
        in_block > 0,
        "stepping out of process_all goes on in the block"
    );
}

/// A debugging session as a user would run it: deferred, conditional and
/// hit-count breakpoints in packages, a type body and a trigger, break on
/// exceptions, stepping in, over and out, and values at every stop.
#[test]
fn order_processing_walkthrough() {
    let Some((_lock, conn, s)) = setup() else {
        return;
    };
    let h = Harness::new();
    let load_done = s.at("load_done");
    let line_amount = s.at("line_amount");
    let money_plus = s.at("money_plus");
    let trigger = s.at("trigger");
    let no_code = s.at("no_code");
    // None of these units is loaded yet: the breakpoints bind on load.
    for bp in [&load_done, &line_amount, &money_plus, &trigger, &no_code] {
        h.send(DebugCommand::SetBreakpoint(bp.clone()));
    }
    h.send(DebugCommand::SetBreakpointOptions(
        line_amount.clone(),
        BreakpointOptions {
            condition: Some("p_line.qty >= 5".into()),
            hit_count: None,
        },
    ));
    h.send(DebugCommand::SetBreakpointOptions(
        money_plus.clone(),
        BreakpointOptions {
            condition: None,
            hit_count: Some(3),
        },
    ));
    h.send(DebugCommand::BreakOnException(true));
    h.send(DebugCommand::SetWatches(vec![
        "g_processed".into(),
        "dbgx_pricing.g_calls".into(),
        "dbgx_pricing.g_rates('L2')".into(),
        "dbgx_types.c_vat".into(),
        "dbgx_types.g_trace".into(),
        "l_order.lines(3).product".into(),
        "l_order.lines(i).product".into(),
        "l_order.customer.name".into(),
    ]));
    let target = run_with_output(h.debugger.port(), RUN);

    // --- order 10: loaded --------------------------------------------------
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(stop.reason, StopReason::Breakpoint);
    assert_eq!(
        (&stop.unit, stop.line),
        (&load_done.unit, Some(load_done.line))
    );
    assert_eq!(
        methods(&stop),
        ["LOAD_ORDER", "PROCESS", "PROCESS_ALL", "BLOCK"]
    );
    let caller = &stop.stack[1].location;
    assert_eq!(
        caller.line,
        Some(s.at("process_start").line),
        "where process called load_order"
    );
    let v = &stop.variables;
    assert_eq!(val(v, "P_ID"), Some("10"));
    assert_eq!(val(v, "I"), Some("3"));
    assert_eq!(val(v, "L_ORDER.HEADER.ID"), Some("10"));
    assert_eq!(val(v, "L_ORDER.HEADER.STATUS"), Some("NEW"));
    assert_eq!(val(v, "L_ORDER.HEADER.TOTAL"), None);
    assert_eq!(val(v, "L_ORDER.CUSTOMER.NAME"), Some("ACME"));
    assert_eq!(val(v, "L_ORDER.CUSTOMER.VIP"), Some("Y"));
    assert_eq!(val(v, "L_ORDER.LINES"), Some("3 element(s)"));
    assert_eq!(var(v, "L_ORDER.LINES.(2)").type_name, "T_LINE");
    assert_eq!(val(v, "L_ORDER.LINES.(2).PRODUCT"), Some("Gadget"));
    assert_eq!(val(v, "L_ORDER.LINES.(2).QTY"), Some("3"));
    assert_eq!(val(v, "L_ORDER.LINES.(3).PRICE"), Some("4.5"));
    assert_eq!(val(v, "L_ORDER.LINES.(3).AMOUNT"), None);
    assert_eq!(val(v, "L_ORDER.NET.AMOUNT"), Some("0"));
    assert_eq!(val(v, "L_ORDER.NET.CURRENCY"), Some("CHF"));
    assert!(
        val(v, "L_ORDER.TAGS").is_some(),
        "SQL collections aren't NULL"
    );
    let w = &stop.watches;
    assert_eq!(watch(w, "g_processed"), Ok(Some("0".into())));
    assert!(
        watch(w, "dbgx_pricing.g_calls").is_err(),
        "pricing isn't loaded yet"
    );
    assert_eq!(
        watch(w, "l_order.lines(3).product"),
        Ok(Some("Cable".into()))
    );
    assert_eq!(
        watch(w, "l_order.lines(i).product"),
        Ok(Some("Cable".into())),
        "i = 3"
    );
    assert_eq!(watch(w, "l_order.customer.name"), Ok(Some("ACME".into())));
    // Globals of a package without a body (process_all set one).
    assert_eq!(watch(w, "dbgx_types.c_vat"), Ok(Some(".081".into())));
    assert_eq!(watch(w, "dbgx_types.g_trace"), Ok(Some("true".into())));

    // The callers' frames have their own variables, and watches are
    // evaluated in the selected frame.
    assert_eq!(stop.frame, 0);
    let (v, w) = h.select(3);
    assert_eq!(methods(&stop)[3], "BLOCK");
    assert_eq!(val(&v, "L_COUNT"), None);
    assert!(watch(&w, "l_order.customer.name").is_err());
    let (v, w) = h.select(1);
    assert_eq!(val(&v, "P_ID"), Some("10"));
    assert_eq!(val(&v, "L_TOTAL"), Some("0"));
    assert_eq!(val(&v, "L_DISCOUNT"), None);
    assert_eq!(
        val(&v, "L_ORDER.CUSTOMER.NAME"),
        None,
        "load_order hasn't returned yet"
    );
    assert_eq!(watch(&w, "l_order.customer.name"), Ok(None));
    assert_eq!(watch(&w, "g_processed"), Ok(Some("0".into())));
    assert_eq!(watch(&w, "dbgx_types.c_vat"), Ok(Some(".081".into())));

    // Step over the end of load_order: back in process, which now has it.
    // A new stop selects the innermost frame again.
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    assert_eq!(
        (stop.reason, stop.line),
        (StopReason::Step, Some(load_done.line + 1))
    );
    assert_eq!(stop.frame, 0);
    assert_eq!(methods(&stop)[0], "LOAD_ORDER");
    assert_eq!(
        watch(&stop.watches, "l_order.customer.name"),
        Ok(Some("ACME".into()))
    );
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(methods(&stop), ["PROCESS", "PROCESS_ALL", "BLOCK"]);
    assert_eq!(stop.line, Some(s.at("process_loaded").line));
    assert_eq!(
        val(&stop.variables, "L_ORDER.LINES.(1).PRODUCT"),
        Some("Widget")
    );
    assert_eq!(val(&stop.variables, "L_TOTAL"), Some("0"));
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    assert_eq!(stop.line, Some(s.at("process_line").line));

    // Step into the first call to pricing: its initialization runs first.
    h.send(DebugCommand::StepInto);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(stop.unit, line_amount.unit);
    assert_eq!(methods(&stop)[..2], ["<clinit>", "PROCESS"]);
    // Loading pricing bound its breakpoints; one without code on its line
    // moved to the next line with code, the entry of line_amount.
    let moved = h.bound.borrow().get(&no_code).cloned();
    assert_eq!(moved, Some(Ok(s.at("line_amount_entry").line)));
    h.send(DebugCommand::ClearBreakpoint(no_code.clone()));
    h.send(DebugCommand::StepOut);
    let (mut stop, _) = h.stop_full();
    eprintln!("after init: {}", s.describe(&stop));
    // Then into line_amount. Its conditional breakpoint doesn't stop the
    // step there, and isn't counted (qty 2).
    while methods(&stop)[0] != "LINE_AMOUNT" {
        assert!(stop.stack.len() >= 3, "{}", s.describe(&stop));
        h.send(DebugCommand::StepInto);
        stop = h.stop_full().0;
        eprintln!("  {}", s.describe(&stop));
    }
    assert_eq!(
        methods(&stop),
        ["LINE_AMOUNT", "PROCESS", "PROCESS_ALL", "BLOCK"]
    );
    assert_eq!(val(&stop.variables, "P_LINE.PRODUCT"), Some("Widget"));
    assert_eq!(val(&stop.variables, "P_LINE.QTY"), Some("2"));
    assert_eq!(val(&stop.variables, "P_LINE.AMOUNT"), None);
    assert_eq!(
        watch(&stop.watches, "dbgx_pricing.g_calls"),
        Ok(Some("0".into()))
    );
    assert_eq!(
        watch(&stop.watches, "dbgx_pricing.g_rates('L2')"),
        Ok(Some(".02".into())),
        "initialized by the package's begin section"
    );
    assert!(
        watch(&stop.watches, "l_order.customer.name").is_err(),
        "a variable of process, not of line_amount"
    );
    let (v, w) = h.select(1);
    assert_eq!(val(&v, "L_ORDER.CUSTOMER.NAME"), Some("ACME"));
    assert_eq!(watch(&w, "l_order.customer.name"), Ok(Some("ACME".into())));
    assert_eq!(
        watch(&w, "dbgx_pricing.g_calls"),
        Ok(Some("0".into())),
        "another package's globals from any frame"
    );
    // Step until the line's amount is assigned back in process.
    while stop.line != Some(s.at("process_sum").line) {
        assert!(stop.stack.len() >= 3, "{}", s.describe(&stop));
        h.send(DebugCommand::StepOver);
        stop = h.stop_full().0;
        eprintln!("  {}", s.describe(&stop));
    }
    assert_eq!(methods(&stop)[0], "PROCESS");
    assert_eq!(stop.frame, 0);
    assert_eq!(
        val(&stop.variables, "L_ORDER.LINES.(1).AMOUNT"),
        Some("99.9")
    );
    assert_eq!(val(&stop.variables, "L_TOTAL"), Some("0"));
    assert_eq!(h.hits.borrow().get(&line_amount), None);

    // --- the conditional breakpoint: line 3 of order 10 (qty 10) -------------
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(stop.reason, StopReason::Breakpoint);
    assert_eq!(
        (&stop.unit, stop.line),
        (&line_amount.unit, Some(line_amount.line))
    );
    assert_eq!(val(&stop.variables, "P_LINE.PRODUCT"), Some("Cable"));
    assert_eq!(val(&stop.variables, "P_LINE.QTY"), Some("10"));
    assert_eq!(
        watch(&stop.watches, "dbgx_pricing.g_calls"),
        Ok(Some("3".into()))
    );
    assert_eq!(
        h.hits.borrow().get(&line_amount),
        Some(&1),
        "qty 2 and 3 didn't count"
    );
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    assert_eq!(stop.line, Some(s.at("line_amount_return").line));
    assert_eq!(val(&stop.variables, "L_AMOUNT"), Some("45"));

    // Stepping over a statement whose trigger has a breakpoint stops in the
    // trigger; continuing then doesn't stop where the step would have.
    let update = s.at("process_update");
    h.send(DebugCommand::SetBreakpoint(update.clone()));
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(
        (stop.reason, stop.line),
        (StopReason::Breakpoint, Some(update.line))
    );
    assert_eq!(
        val(&stop.variables, "L_ORDER.NET.AMOUNT"),
        Some("207.625625")
    );
    assert_eq!(val(&stop.variables, "L_DISCOUNT"), Some("12.804375"));
    assert_eq!(val(&stop.variables, "L_TOTAL"), Some("204.87"));
    h.send(DebugCommand::ClearBreakpoint(update));
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(stop.reason, StopReason::Breakpoint);
    assert_eq!((&stop.unit, stop.line), (&trigger.unit, Some(trigger.line)));
    assert_eq!(methods(&stop)[..2], ["DBGX_ORDERS_BU", "PROCESS"]);
    assert_eq!(
        s.line(&stop.unit, stop.line),
        "l_msg := :old.status || ' -> ' || :new.status; -- @trigger"
    );
    assert_eq!(val(&stop.variables, "OLD.STATUS"), Some("NEW"));
    assert_eq!(val(&stop.variables, "NEW.STATUS"), Some("DONE"));
    assert_eq!(val(&stop.variables, "NEW.ID"), Some("10"));
    eprintln!("new.total: {:?}", val(&stop.variables, "NEW.TOTAL"));
    assert_eq!(val(&stop.variables, "L_MSG"), None);
    h.send(DebugCommand::Resume);

    // --- order 11: credit limit exceeded -------------------------------------
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(
        (stop.reason, stop.line),
        (StopReason::Breakpoint, Some(load_done.line)),
        "the abandoned step over the update didn't fire"
    );
    assert_eq!(val(&stop.variables, "P_ID"), Some("11"));
    assert_eq!(watch(&stop.watches, "g_processed"), Ok(Some("1".into())));
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    eprintln!("{} {:?}", s.describe(&stop), stop.exception);
    assert_eq!(stop.reason, StopReason::Exception);
    assert_eq!(stop.line, Some(s.at("raise").line));
    assert_eq!(methods(&stop)[..2], ["CHECK_CREDIT", "PROCESS"]);
    assert_eq!(stop.exception.as_deref(), Some("ORA-20010"));
    assert_eq!(
        val(&stop.variables, "P_ORDER.CUSTOMER.NAME"),
        Some("Globex")
    );
    assert_eq!(val(&stop.variables, "P_ORDER.NET.AMOUNT"), Some("162.15"));
    // The handler in process catches it.
    let mut stop = stop;
    while stop.line != Some(s.at("process_hold").line) {
        h.send(DebugCommand::StepOver);
        stop = h.stop_full().0;
        eprintln!("  {} {:?}", s.describe(&stop), stop.exception);
        assert_ne!(stop.reason, StopReason::Exception, "one stop per raise");
    }
    assert_eq!(methods(&stop)[0], "PROCESS");
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    assert_eq!((&stop.unit, stop.line), (&trigger.unit, Some(trigger.line)));
    assert_eq!(val(&stop.variables, "NEW.STATUS"), Some("HOLD"));
    assert_eq!(h.hits.borrow().get(&trigger), Some(&2));

    // --- order 12: recursion, a breakpoint set while stopped, hit counts -----
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    assert_eq!(
        (stop.line, val(&stop.variables, "P_ID")),
        (Some(load_done.line), Some("12"))
    );
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    assert_eq!(
        (&stop.unit, stop.line),
        (&line_amount.unit, Some(line_amount.line))
    );
    assert_eq!(val(&stop.variables, "P_LINE.QTY"), Some("5"));
    assert_eq!(h.hits.borrow().get(&line_amount), Some(&2));
    let tiered = s.at("tiered");
    h.send(DebugCommand::SetBreakpoint(tiered.clone()));
    h.send(DebugCommand::SetBreakpointOptions(
        tiered.clone(),
        BreakpointOptions {
            condition: Some("p_level = 3".into()),
            hit_count: None,
        },
    ));
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!((&stop.unit, stop.line), (&tiered.unit, Some(tiered.line)));
    assert_eq!(
        methods(&stop),
        [
            "TIERED",
            "TIERED",
            "TIERED",
            "DISCOUNT",
            "PROCESS",
            "PROCESS_ALL",
            "BLOCK"
        ]
    );
    assert_eq!(val(&stop.variables, "P_LEVEL"), Some("3"));
    assert_eq!(val(&stop.variables, "P_AMOUNT"), Some("62.4375"));
    assert_eq!(val(&stop.variables, "L_RATE"), Some(".01"));
    assert_eq!(
        h.hits.borrow().get(&tiered),
        Some(&1),
        "levels 1 and 2 didn't count"
    );
    // Each level of the recursion has its own values; a watch added while
    // a caller is selected is evaluated there.
    let (v, _) = h.select(2);
    assert_eq!(val(&v, "P_LEVEL"), Some("1"));
    assert_eq!(val(&v, "P_AMOUNT"), Some("249.75"));
    let (v, _) = h.select(1);
    assert_eq!(val(&v, "P_LEVEL"), Some("2"));
    assert_eq!(val(&v, "L_RATE"), Some(".02"));
    h.send(DebugCommand::SetWatches(vec![
        "p_level".into(),
        "g_calls".into(),
    ]));
    let w = h.watches();
    assert_eq!(watch(&w, "p_level"), Ok(Some("2".into())));
    assert_eq!(watch(&w, "g_calls"), Ok(Some("5".into())));
    // Out of the innermost call: the caller's own values.
    h.send(DebugCommand::StepOut);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(methods(&stop)[..3], ["TIERED", "TIERED", "DISCOUNT"]);
    // Oracle stops on the caller's next line once the call returned.
    assert_eq!(stop.line, Some(s.at("tiered_recurse").line + 1));
    assert_eq!(val(&stop.variables, "P_LEVEL"), Some("2"));
    assert_eq!(val(&stop.variables, "P_AMOUNT"), Some("124.875"));
    h.send(DebugCommand::ClearBreakpoint(tiered));
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(
        (&stop.unit, stop.line),
        (&money_plus.unit, Some(money_plus.line))
    );
    assert_eq!(methods(&stop)[..3], ["PLUS", "APPLY_TAX", "PROCESS"]);
    assert_eq!(
        h.hits.borrow().get(&money_plus),
        Some(&3),
        "stops from the third call on"
    );
    assert_eq!(val(&stop.variables, "SELF.AMOUNT"), Some("234.140625"));
    assert_eq!(val(&stop.variables, "P_OTHER.AMOUNT"), Some("18.97"));
    assert_eq!(val(&stop.variables, "P_OTHER.CURRENCY"), Some("CHF"));
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    assert_eq!((&stop.unit, stop.line), (&trigger.unit, Some(trigger.line)));
    assert_eq!(val(&stop.variables, "NEW.ID"), Some("12"));
    h.send(DebugCommand::ClearBreakpoint(trigger.clone()));
    h.send(DebugCommand::Resume);
    assert!(h.next_stop().is_none(), "nothing else stops");

    let outcome = target.join().unwrap();
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.output, ["processed 2"]);
    // The debugger didn't change what the code did.
    let rows: Vec<(i64, String, Option<String>)> = conn
        .query_as::<(i64, String, Option<String>)>(
            "select id, status, to_char(total) from dbgx_orders order by id",
            &[],
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            (10, "DONE".into(), Some("207.63".into())),
            (11, "HOLD".into(), None),
            (12, "DONE".into(), Some("253.11".into())),
        ]
    );
}

/// Pause while the target runs, and a breakpoint set while it runs.
#[test]
fn pause_and_breakpoints_while_running() {
    let Some((_lock, _conn, s)) = setup() else {
        return;
    };
    let h = Harness::new();
    let target = run_target(h.debugger.port(), "begin dbgx_orders_api.spin(200); end;");
    loop {
        if let DebugEvent::Connected { .. } = h.next() {
            break;
        }
    }
    thread::sleep(Duration::from_millis(1500));
    h.send(DebugCommand::Pause);
    let (stop, _) = h.stop_full();
    eprintln!("paused: {} {:?}", s.describe(&stop), methods(&stop));
    assert_eq!(stop.reason, StopReason::Pause);
    assert!(methods(&stop).contains(&"SPIN"), "{:?}", methods(&stop));
    // Stepping from code without debug information (DBMS_SESSION.SLEEP)
    // gets back to the caller.
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    eprintln!("stepped: {}", s.describe(&stop));
    assert_eq!(methods(&stop)[0], "SPIN", "{}", s.describe(&stop));
    assert!(stop.line.is_some());
    h.send(DebugCommand::Resume);
    thread::sleep(Duration::from_millis(500));
    let spin = s.at("spin");
    h.send(DebugCommand::SetBreakpoint(spin.clone()));
    let (stop, _) = h.stop_full();
    assert_eq!(
        (stop.reason, &stop.unit, stop.line),
        (StopReason::Breakpoint, &spin.unit, Some(spin.line))
    );
    let round: u32 = val(&stop.variables, "L_ROUND").unwrap().parse().unwrap();
    assert!(
        round > 10,
        "ran a while before the breakpoint was set: {round}"
    );
    h.send(DebugCommand::Resume);
    let (stop, _) = h.stop_full();
    let next: u32 = val(&stop.variables, "L_ROUND").unwrap().parse().unwrap();
    assert_eq!(next, round + 1, "each loop pass stops");
    h.send(DebugCommand::ClearBreakpoint(spin));
    h.send(DebugCommand::Terminate);
    h.until_terminated();
    assert!(target.join().unwrap().is_err(), "terminated");
}

/// Stepping into a unit compiled without debug information steps over it:
/// Oracle reports no lines there.
#[test]
fn step_into_code_without_debug_info() {
    let Some((_lock, conn, s)) = setup() else {
        return;
    };
    conn.execute("alter package dbgx_pricing compile body", &[])
        .unwrap();
    let body = Unit::new(OWNER, "DBGX_PRICING", UnitType::PackageBody);
    assert!(!target::has_debug_info(&conn, &body).unwrap());
    let h = Harness::new();
    let call = s.at("process_line");
    h.send(DebugCommand::SetBreakpoint(call.clone()));
    let target = run_target(h.debugger.port(), RUN);
    let (stop, _) = h.stop_full();
    assert_eq!(stop.line, Some(call.line));
    h.send(DebugCommand::StepInto);
    let (stop, _) = h.stop_full();
    assert_eq!(methods(&stop)[0], "PROCESS", "{}", s.describe(&stop));
    assert_eq!(stop.line, Some(s.at("process_sum").line));
    // The body was loaded during the step: the debugger says why the step
    // didn't go in.
    assert_eq!(*h.stepped_over.borrow(), [body]);
    h.send(DebugCommand::ClearBreakpoint(call));
    h.send(DebugCommand::Resume);
    h.until_terminated();
    target.join().unwrap().expect("target ran to completion");
}

/// The compiler messages of a unit, as `ALL_ERRORS` has them.
fn messages(conn: &Connection, unit: &Unit) -> Vec<String> {
    conn.query_as::<String>(
        "select text from user_errors where name = :1 and type = :2 order by sequence",
        &[&unit.name, &unit.unit_type.dictionary_name()],
    )
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// What program windows do with "Compile with debug information": the
/// `CREATE` runs with `PLSQL_DEBUG` on, and the unit then has debug
/// information, no new compiler messages but Oracle's notes about the
/// option, and can be stepped into.
#[test]
fn step_into_unit_compiled_with_debug_information() {
    let Some((_lock, conn, s)) = setup() else {
        return;
    };
    let body = Unit::new(OWNER, "DBGX_PRICING", UnitType::PackageBody);
    let source = s.sources[&body].clone();
    // All warnings on, to see every message.
    conn.execute("alter session set plsql_warnings = 'ENABLE:ALL'", &[])
        .unwrap();
    conn.execute(&source, &[]).unwrap();
    assert!(!target::has_debug_info(&conn, &body).unwrap());
    let plain = messages(&conn, &body);
    let previous = target::set_compile_debug(&conn, true).unwrap();
    assert!(!previous);
    conn.execute(&source, &[]).unwrap();
    assert!(target::set_compile_debug(&conn, previous).unwrap());
    assert!(target::has_debug_info(&conn, &body).unwrap());
    let debug = messages(&conn, &body);
    eprintln!("plain: {plain:?}\ndebug: {debug:?}");
    let new: Vec<&String> = debug.iter().filter(|m| !plain.contains(m)).collect();
    assert!(!new.is_empty(), "Oracle notes PLSQL_DEBUG");
    assert!(
        new.iter().all(|m| target::is_debug_notice(m)),
        "only notes about the option: {new:?}"
    );
    assert!(
        debug.iter().all(|m| m.starts_with("PLW-")),
        "no errors: {debug:?}"
    );
    // The session compiles without debug information again.
    let setting: String = conn
        .query_row_as(
            "select value from v$parameter where name = 'plsql_debug'",
            &[],
        )
        .unwrap();
    assert_eq!(setting, "FALSE");

    let h = Harness::new();
    let call = s.at("process_line");
    h.send(DebugCommand::SetBreakpoint(call.clone()));
    let target = run_target(h.debugger.port(), RUN);
    let (stop, _) = h.stop_full();
    assert_eq!(stop.line, Some(call.line));
    h.send(DebugCommand::StepInto);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(stop.unit, body);
    assert_eq!(methods(&stop)[..2], ["<clinit>", "PROCESS"]);
    assert!(h.stepped_over.borrow().is_empty());
    h.send(DebugCommand::ClearBreakpoint(call));
    h.send(DebugCommand::Resume);
    h.until_terminated();
    target.join().unwrap().expect("target ran to completion");
}

/// Globals of a package the session hasn't instantiated yet have no values
/// (Oracle's agent has no objects for them): the debugger says so instead
/// of showing NULL. Once instantiated, NULL is NULL.
#[test]
fn globals_of_a_package_not_instantiated_yet() {
    let Some((_lock, _conn, s)) = setup() else {
        return;
    };
    let h = Harness::new();
    let trace = s.at("process_all_trace");
    h.send(DebugCommand::SetBreakpoint(trace.clone()));
    h.send(DebugCommand::SetWatches(vec![
        "dbgx_types.c_vat".into(),
        "dbgx_types.g_trace".into(),
        "dbgx_types.g_note".into(),
        "dbgx_pricing.g_calls".into(),
    ]));
    let target = run_target(h.debugger.port(), RUN);
    let (stop, _) = h.stop_full();
    assert_eq!(stop.line, Some(trace.line));
    // Loaded for its types, but nothing has used it yet.
    for expression in [
        "dbgx_types.c_vat",
        "dbgx_types.g_trace",
        "dbgx_types.g_note",
    ] {
        let error = watch(&stop.watches, expression).unwrap_err();
        assert!(
            error.starts_with("dbgx_types isn't instantiated in this session yet"),
            "{expression}: {error}"
        );
    }
    let error = watch(&stop.watches, "dbgx_pricing.g_calls").unwrap_err();
    assert!(error.contains("isn't loaded yet"), "{error}");
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    assert_eq!(
        watch(&stop.watches, "dbgx_types.c_vat"),
        Ok(Some(".081".into()))
    );
    assert_eq!(
        watch(&stop.watches, "dbgx_types.g_trace"),
        Ok(Some("true".into()))
    );
    assert_eq!(watch(&stop.watches, "dbgx_types.g_note"), Ok(None));
    h.send(DebugCommand::ClearBreakpoint(trace));
    h.send(DebugCommand::Resume);
    h.until_terminated();
    target.join().unwrap().expect("target ran to completion");
}

/// A record whose type is declared in a package spec compiled without debug
/// information comes through as an opaque object: Oracle's agent can't
/// describe it. `opaque_reasons` names the package from the declaration.
#[test]
fn records_of_a_spec_without_debug_information() {
    let Some((_lock, conn, s)) = setup() else {
        return;
    };
    let types = Unit::new(OWNER, "DBGX_TYPES", UnitType::Package);
    let api = Unit::new(OWNER, "DBGX_ORDERS_API", UnitType::PackageBody);
    conn.execute("alter package dbgx_types compile specification", &[])
        .unwrap();
    assert!(!target::has_debug_info(&conn, &types).unwrap());
    assert!(target::has_debug_info(&conn, &api).unwrap(), "still");

    let h = Harness::new();
    let call = s.at("process_line");
    h.send(DebugCommand::SetBreakpoint(call.clone()));
    let run = run_target(h.debugger.port(), RUN);
    let (stop, _) = h.stop_full();
    assert_eq!(stop.line, Some(call.line));
    let order = var(&stop.variables, "L_ORDER");
    assert_eq!(order.type_name, "OPAQUE");
    assert!(order.children.is_empty());
    assert_eq!(val(&stop.variables, "L_TOTAL"), Some("0"));
    let names = ["L_ORDER".to_string(), "L_TOTAL".to_string()];
    assert_eq!(
        target::opaque_reasons(&conn, &api, stop.line, &names, None).unwrap(),
        [(
            "L_ORDER".to_string(),
            "T_ORDER is declared in DBGX_TYPES, compiled without debug information".to_string()
        )]
    );
    h.send(DebugCommand::ClearBreakpoint(call.clone()));
    h.send(DebugCommand::Resume);
    h.until_terminated();
    run.join().unwrap().expect("target ran to completion");

    // With debug information there is nothing to explain (and the record
    // has its fields again, see order_processing_walkthrough).
    target::compile_debug(&conn, &types).unwrap();
    assert!(
        target::opaque_reasons(&conn, &api, Some(call.line), &names, None)
            .unwrap()
            .is_empty()
    );
}

/// A value is opaque when its type was compiled without debug information,
/// wherever it's declared: in the unit's own package spec (debugging
/// compiles only the body), or as a SQL type, also SYS's or by a synonym.
/// A type declared in the debugged code itself is readable (#166).
#[test]
fn opaque_types_of_the_own_spec_and_sql_types() {
    let Some((_lock, conn, _s)) = setup() else {
        return;
    };
    for sql in [
        "create or replace synonym dbgx_list for sys.odcivarchar2list",
        "create or replace package dbgx_opaque as
  type t_arr is table of number index by pls_integer;
  procedure run;
end;",
        "create or replace package body dbgx_opaque as
  procedure run is
    type t_local is table of number index by pls_integer;
    l_arr   t_arr;
    l_codes dbgx_codes;
    l_tags  sys.odcivarchar2list;
    l_list  dbgx_list;
    l_local t_local;
  begin
    l_arr(1) := 1;
    l_codes := dbgx_codes(7);
    l_tags := sys.odcivarchar2list('a');
    l_list := dbgx_list('b');
    l_local(1) := 2;
    null;
  end;
end;",
        "alter package dbgx_opaque compile debug body",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let spec = Unit::new(OWNER, "DBGX_OPAQUE", UnitType::Package);
    let body = Unit::new(OWNER, "DBGX_OPAQUE", UnitType::PackageBody);
    assert!(!target::has_debug_info(&conn, &spec).unwrap());
    assert!(target::has_debug_info(&conn, &body).unwrap());

    let h = Harness::new();
    let stop_at = Breakpoint {
        unit: body.clone(),
        line: 15,
    };
    h.send(DebugCommand::SetBreakpoint(stop_at.clone()));
    let run = run_target(h.debugger.port(), "begin dbgx_opaque.run; end;");
    let (stop, _) = h.stop_full();
    assert_eq!(stop.line, Some(15));
    let names: Vec<String> = ["L_ARR", "L_CODES", "L_TAGS", "L_LIST"]
        .map(String::from)
        .into();
    for name in &names {
        assert_eq!(var(&stop.variables, name).type_name, "OPAQUE", "{name}");
    }
    assert_eq!(val(&stop.variables, "L_LOCAL.(1)"), Some("2"));
    let reason = |r: &str| format!("{r}, compiled without debug information");
    let mut asked = names.clone();
    asked.push("L_LOCAL".into());
    assert_eq!(
        target::opaque_reasons(&conn, &body, stop.line, &asked, None).unwrap(),
        [
            (
                "L_ARR".to_string(),
                reason("T_ARR is declared in the specification of DBGX_OPAQUE")
            ),
            (
                "L_CODES".to_string(),
                "DBGX_CODES is a SQL type compiled without debug information".to_string()
            ),
            (
                "L_TAGS".to_string(),
                "SYS.ODCIVARCHAR2LIST is a SQL type compiled without debug information".to_string()
            ),
            (
                "L_LIST".to_string(),
                "SYS.ODCIVARCHAR2LIST is a SQL type compiled without debug information".to_string()
            ),
        ]
    );
    h.send(DebugCommand::ClearBreakpoint(stop_at.clone()));
    h.send(DebugCommand::Resume);
    h.until_terminated();
    run.join().unwrap().expect("target ran to completion");

    // Compiled with debug information, the spec's and the SQL type's values
    // have their contents.
    target::compile_debug(&conn, &spec).unwrap();
    conn.execute("alter type dbgx_codes compile debug specification", &[])
        .unwrap();
    assert!(
        target::opaque_reasons(&conn, &body, Some(15), &names[..2], None)
            .unwrap()
            .is_empty()
    );
    let h = Harness::new();
    h.send(DebugCommand::SetBreakpoint(stop_at.clone()));
    let run = run_target(h.debugger.port(), "begin dbgx_opaque.run; end;");
    let (stop, _) = h.stop_full();
    assert_eq!(val(&stop.variables, "L_ARR.(1)"), Some("1"));
    assert_eq!(val(&stop.variables, "L_CODES.(1)"), Some("7"));
    assert_eq!(var(&stop.variables, "L_TAGS").type_name, "OPAQUE", "SYS's");
    h.send(DebugCommand::ClearBreakpoint(stop_at));
    h.send(DebugCommand::Resume);
    h.until_terminated();
    run.join().unwrap().expect("target ran to completion");
    for sql in [
        "drop package dbgx_opaque",
        "drop synonym dbgx_list",
        "alter type dbgx_codes compile",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
}

/// Recompiling, as the object browser's Compile and the invalid objects
/// window do, keeps debug information: valid units and invalid ones.
#[test]
fn recompiling_keeps_debug_information() {
    let Some((_lock, conn, _s)) = setup() else {
        return;
    };
    let compile = |unit: &Unit| {
        let id = pythia_db::recompile::ObjectId::new(
            &unit.owner,
            &unit.name,
            unit.unit_type.dictionary_name(),
        );
        let sql = pythia_db::recompile::compile_sql(&id).unwrap();
        conn.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    };
    for unit in Scenario::units() {
        compile(&unit);
        assert!(target::has_debug_info(&conn, &unit).unwrap(), "{unit}");
    }
    let status = |unit: &Unit| -> String {
        conn.query_row_as(
            "select status from user_objects where object_name = :1 and object_type = :2",
            &[&unit.name, &unit.unit_type.dictionary_name()],
        )
        .unwrap()
    };
    let api = Unit::new(OWNER, "DBGX_ORDERS_API", UnitType::PackageBody);
    conn.execute("alter table dbgx_order_lines add (pythia_tmp number)", &[])
        .unwrap();
    conn.execute("alter table dbgx_order_lines drop (pythia_tmp)", &[])
        .unwrap();
    assert_eq!(status(&api), "INVALID");
    compile(&api);
    assert_eq!(status(&api), "VALID");
    assert!(target::has_debug_info(&conn, &api).unwrap());
}

/// Debug… on a package function: the session stops when execution first
/// enters it, at its header with the parameters set, without a breakpoint
/// of the user's; later calls (it recurses, and runs for every VIP order)
/// don't stop again.
#[test]
fn stop_on_entry() {
    let Some((_lock, _conn, s)) = setup() else {
        return;
    };
    let pricing = Unit::new(OWNER, "DBGX_PRICING", UnitType::PackageBody);
    let h = Harness::new();
    h.send(DebugCommand::BreakOnEntry {
        unit: pricing.clone(),
        subprogram: "tiered".into(),
    });
    let target = run_target(h.debugger.port(), RUN);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!((stop.reason, &stop.unit), (StopReason::Entry, &pricing));
    assert_eq!(methods(&stop)[..2], ["TIERED", "DISCOUNT"]);
    assert!(
        s.line(&stop.unit, stop.line)
            .starts_with("function tiered("),
        "the function's header"
    );
    assert_eq!(val(&stop.variables, "P_LEVEL"), Some("1"));
    h.send(DebugCommand::StepOver);
    let (stop, _) = h.stop_full();
    eprintln!("{}", s.describe(&stop));
    assert_eq!(stop.reason, StopReason::Step);
    h.send(DebugCommand::Resume);
    assert!(h.next_stop().is_none(), "stops once");
    target.join().unwrap().expect("target ran to completion");
}
