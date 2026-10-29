//! Shared helpers for debugging against a real database.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use pythia_db::oracle::Connection;
use pythia_debugger::target;
use pythia_debugger::{
    Breakpoint, DebugCommand, DebugEvent, Debugger, Frame, StopReason, Unit, Variable, Watch,
};

pub fn connect(var: &str) -> Option<Connection> {
    let spec = std::env::var(var).ok()?;
    let (cred, db) = spec.rsplit_once('@')?;
    let (user, pw) = cred.split_once('/')?;
    Some(Connection::connect(user, pw, db).expect("connect"))
}

/// How the database reaches this machine.
pub fn host() -> String {
    std::env::var("PYTHIA_DEBUG_HOST").unwrap_or_else(|_| "host.containers.internal".into())
}

/// A stop with everything the debugger reports.
#[derive(Debug)]
pub struct Stop {
    pub reason: StopReason,
    pub unit: Unit,
    pub line: Option<u32>,
    pub stack: Vec<Frame>,
    /// The frame `variables` and `watches` belong to.
    pub frame: usize,
    pub variables: Vec<Variable>,
    pub watches: Vec<Watch>,
    pub exception: Option<String>,
}

pub struct Harness {
    pub debugger: Debugger,
    wake: mpsc::Receiver<()>,
    /// Hits per breakpoint, as reported so far.
    pub hits: RefCell<BTreeMap<Breakpoint, u32>>,
    /// Where each breakpoint was bound (or why not), as reported so far.
    pub bound: RefCell<BTreeMap<Breakpoint, Result<u32, String>>>,
    /// Units step into went over, as reported so far.
    pub stepped_over: RefCell<Vec<Unit>>,
}

impl Harness {
    pub fn new() -> Self {
        let (tx, wake) = mpsc::channel();
        let debugger = Debugger::listen("0.0.0.0:0", move || {
            let _ = tx.send(());
        })
        .expect("listen");
        Self {
            debugger,
            wake,
            hits: RefCell::default(),
            bound: RefCell::default(),
            stepped_over: RefCell::default(),
        }
    }

    pub fn send(&self, command: DebugCommand) {
        self.debugger.send(command);
    }

    pub fn next(&self) -> DebugEvent {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(e) = self.debugger.try_recv() {
                eprintln!("event: {}", short(&e));
                match &e {
                    DebugEvent::BreakpointHit { breakpoint, hits } => {
                        self.hits.borrow_mut().insert(breakpoint.clone(), *hits);
                    }
                    DebugEvent::BreakpointBound {
                        breakpoint,
                        line,
                        error,
                    } => {
                        let entry = line.ok_or_else(|| error.clone().unwrap_or_default());
                        self.bound.borrow_mut().insert(breakpoint.clone(), entry);
                    }
                    DebugEvent::SteppedOver(units) => {
                        self.stepped_over.borrow_mut().extend(units.iter().cloned());
                    }
                    _ => {}
                }
                return e;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for a debug event");
            let _ = self.wake.recv_timeout(left.min(Duration::from_millis(200)));
        }
    }

    /// Next suspension: (reason, unit, line, variables, stack depth).
    pub fn stop(&self) -> (StopReason, Unit, Option<u32>, Vec<Variable>, usize) {
        let (stop, depth) = self.stop_full();
        (stop.reason, stop.unit, stop.line, stop.variables, depth)
    }

    pub fn stop_full(&self) -> (Stop, usize) {
        let stop = self
            .next_stop()
            .expect("terminated while waiting for a stop");
        let depth = stop.stack.len();
        (stop, depth)
    }

    /// The next stop, or `None` when the session ends first.
    pub fn next_stop(&self) -> Option<Stop> {
        loop {
            match self.next() {
                DebugEvent::Suspended {
                    reason,
                    location,
                    variables,
                    stack,
                    frame,
                    watches,
                    exception,
                } => {
                    return Some(Stop {
                        reason,
                        unit: location.unit,
                        line: location.line,
                        stack,
                        frame,
                        variables,
                        watches,
                        exception,
                    });
                }
                DebugEvent::Terminated => return None,
                DebugEvent::Error(e) => panic!("debugger error: {e}"),
                _ => {}
            }
        }
    }

    /// Selects a frame of the stopped thread: its variables and watches.
    pub fn select(&self, frame: usize) -> (Vec<Variable>, Vec<Watch>) {
        self.send(DebugCommand::SelectFrame(frame));
        loop {
            match self.next() {
                DebugEvent::FrameSelected {
                    frame: selected,
                    variables,
                    watches,
                } => {
                    assert_eq!(selected, frame);
                    return (variables, watches);
                }
                DebugEvent::Terminated => panic!("terminated while selecting a frame"),
                DebugEvent::Error(e) => panic!("debugger error: {e}"),
                _ => {}
            }
        }
    }

    /// The next watch values sent without a stop (after `SetWatches`).
    pub fn watches(&self) -> Vec<Watch> {
        loop {
            match self.next() {
                DebugEvent::Watches(w) => return w,
                DebugEvent::Terminated => panic!("terminated while waiting for watches"),
                _ => {}
            }
        }
    }

    pub fn until_terminated(&self) {
        loop {
            match self.next() {
                DebugEvent::Terminated => return,
                DebugEvent::Suspended { .. } => self.debugger.send(DebugCommand::Resume),
                _ => {}
            }
        }
    }
}

pub fn short(e: &DebugEvent) -> String {
    match e {
        DebugEvent::Suspended {
            reason,
            location,
            stack,
            ..
        } => {
            format!(
                "suspended {reason:?} at {} line {:?} (depth {})",
                location.unit,
                location.line,
                stack.len()
            )
        }
        other => format!("{other:?}"),
    }
}

pub fn value<'a>(vars: &'a [Variable], name: &str) -> Option<&'a str> {
    vars.iter().find(|v| v.name == name)?.value.as_deref()
}

/// A variable by path, e.g. `L_ORDER.LINES.(2).PRODUCT` (dot-separated
/// names as the debugger reports them).
pub fn var<'a>(vars: &'a [Variable], path: &str) -> &'a Variable {
    let mut parts = path.split('.');
    let first = parts.next().unwrap();
    let mut v = vars
        .iter()
        .find(|v| v.name == first)
        .unwrap_or_else(|| panic!("no {first} in {:?}", names(vars)));
    for part in parts {
        v = v
            .children
            .iter()
            .find(|c| c.name == part)
            .unwrap_or_else(|| panic!("{path}: no {part} in {:?}", names(&v.children)));
    }
    v
}

/// The value of [`var`] (None = NULL).
pub fn val<'a>(vars: &'a [Variable], path: &str) -> Option<&'a str> {
    var(vars, path).value.as_deref()
}

fn names(vars: &[Variable]) -> Vec<&str> {
    vars.iter().map(|v| v.name.as_str()).collect()
}

/// A watch's value: `Ok(value)` (None = NULL) or the error text.
pub fn watch(watches: &[Watch], expression: &str) -> Result<Option<String>, String> {
    let w = watches
        .iter()
        .find(|w| w.expression == expression)
        .unwrap_or_else(|| panic!("no watch {expression}: {watches:?}"));
    w.value.clone().map(|v| v.value)
}

pub fn run_target(
    port: u16,
    block: &'static str,
) -> thread::JoinHandle<Result<(), pythia_db::DbError>> {
    let host = host();
    thread::spawn(move || {
        let conn = connect("PYTHIA_TEST_DB").unwrap();
        target::run_under_debugger(&conn, &host, port, block, false)
    })
}
