//! Debugger UI: a session driver the app hosts, plus an egui panel with
//! toolbar, call stack, variables, watches and breakpoints, and beside them
//! the SQL the stopped session ran with its plans.
//!
//! Keys (chosen not to clash with Pythia's PL/SQL Developer keymap, where F5 is
//! explain plan, F8 execute and F10/Shift+F10 commit/rollback):
//!
//! | Key        | Action                                   |
//! |------------|------------------------------------------|
//! | Ctrl+F9    | Start debugging (handled by the app)     |
//! | F9         | Continue                                 |
//! | F6         | Step over                                |
//! | F7         | Step into                                |
//! | Shift+F7   | Step out                                 |
//! | Shift+F9   | Stop (terminate the call)                |
//! | Ctrl+B     | Toggle breakpoint at cursor (app)        |
//! | Ctrl+F7    | Run to cursor (app)                      |

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use egui::{Color32, Key, KeyboardShortcut, Modifiers, RichText, Ui};
use serde::{Deserialize, Serialize};

use crate::Debugger;
use crate::condition;
use crate::setup::{Ports, Problem};
use crate::sql::{CursorPlan, SessionSql, TargetSession};
use crate::sql_panel::{self, SqlPanel};
use crate::types::{
    Breakpoint, BreakpointOptions, DebugCommand, DebugEvent, Frame, Location, StopReason, Unit,
    UnitType, Variable, Watch,
};
use crate::value;

pub const CONTINUE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F9);
pub const STEP_OVER: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F6);
pub const STEP_INTO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F7);
pub const STEP_OUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::SHIFT, Key::F7);
pub const STOP: KeyboardShortcut = KeyboardShortcut::new(Modifiers::SHIFT, Key::F9);
pub const START: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F9);
pub const TOGGLE_BREAKPOINT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::B);

/// What the app should do for the debugger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugAction {
    /// Run `block` on a dedicated session under the debugger:
    /// `DbRequest::Task { task: target::debug_task(host, port, block,
    /// statistics) }`, after `target::session_task()` on the same session,
    /// whose answer goes to [`DebugSession::set_target_session`].
    RunTarget {
        host: String,
        port: u16,
        block: String,
        statistics: bool,
    },
    /// Execution stopped here: open the unit and mark the line.
    ShowLocation(Location),
    /// Find out why these variables of `unit` (stopped at `line`) have no
    /// contents: `target::opaque_reasons`, then
    /// [`DebugSession::set_opaque_reasons`]. `source` is the block's text
    /// for an anonymous block.
    ExplainOpaque {
        unit: Unit,
        line: Option<u32>,
        names: Vec<String>,
        source: Option<String>,
    },
    /// The session ended: clear execution markers.
    Finished,
    /// Open the debugger setup, saying why (a failed start) if there is a
    /// reason.
    Setup { reason: Option<String> },
    /// Look up the SQL the stopped session ran, on another session:
    /// `sql::session_sql`, then [`DebugSession::sql_found`] with `stop`.
    FindSql { session: TargetSession, stop: u64 },
    /// Load a statement's plan, on another session: `sql::cursor_plan`,
    /// then [`DebugSession::plan_loaded`].
    LoadPlan { sql_id: String, child: i64 },
    /// Open a unit at a line (a statement's), not marking execution there.
    OpenSource(Location),
    /// Ask the assistant about a statement of the SQL panel.
    AnalyzeSql(SqlQuestion),
}

/// What the assistant gets about a statement the debugged session ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlQuestion {
    /// E.g. "statement 7hmwardbdtjab at procedure PYTHIA.P line 12".
    pub title: String,
    pub sql: String,
    /// DBMS_XPLAN's report of the plan and what the panel noted about it.
    pub plan: String,
    /// The plan has actual rows.
    pub measured: bool,
    /// The variables of the stopped frame (`name = value`): the
    /// statement's binds are among them.
    pub variables: Vec<String>,
    /// Where the statement is in the code.
    pub location: Option<Location>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Idle,
    /// Listening; waiting for the target session to connect.
    Waiting,
    Running,
    Suspended {
        reason: StopReason,
    },
    Finished,
}

/// Debugger state for one connection, hosted by the app.
#[derive(Default)]
pub struct DebugSession {
    debugger: Option<Debugger>,
    pub state: State,
    /// Breakpoints, kept across sessions (including disabled ones).
    pub breakpoints: BTreeSet<Breakpoint>,
    /// Breakpoints kept in the list but not set.
    pub disabled: BTreeSet<Breakpoint>,
    /// Conditions and hit counts.
    pub options: BTreeMap<Breakpoint, BreakpointOptions>,
    /// Hits (condition true) in the current session.
    pub hits: BTreeMap<Breakpoint, u32>,
    /// Line a breakpoint actually bound to (or an error), per breakpoint.
    bound: BTreeMap<Breakpoint, Result<u32, String>>,
    pub stack: Vec<Frame>,
    /// The selected call stack frame: `variables`, the watches and hover
    /// values are its. Every stop selects the innermost frame with source.
    pub frame: usize,
    pub variables: Vec<Variable>,
    /// Watched expressions, kept across sessions, and their last values.
    pub watches: Vec<String>,
    pub watch_values: Vec<Watch>,
    pub break_on_exception: bool,
    /// The exception execution stopped at.
    pub exception: Option<String>,
    pub log: Vec<String>,
    /// Text of the "add watch" field.
    new_watch: String,
    /// The breakpoint whose condition is being edited, with the draft
    /// condition and hit count.
    editing: Option<(Breakpoint, String, u32)>,
    /// The block being debugged.
    block: String,
    /// The subprogram the session stops in once, on entry: its unit and
    /// name (see [`DebugCommand::BreakOnEntry`]).
    pub entry: Option<(Unit, String)>,
    /// Why opaque variables have no contents, by unit and variable name.
    opaque_reasons: BTreeMap<(Unit, String), String>,
    /// Opaque variables asked about in this session (each once).
    opaque_asked: BTreeSet<(Unit, String)>,
    /// The value being changed in the Variables or Watches panel.
    value_edit: Option<ValueEdit>,
    /// Why the last change of a value failed: its path and the reason.
    value_error: Option<(String, String)>,
    /// Where the database connects to (host and port), while it should.
    target: Option<(String, u16)>,
    /// When the debugger started waiting for the session.
    waiting_since: Option<Instant>,
    /// Why the last start failed to connect, if it was a setup problem.
    pub problem: Option<String>,
    /// The debugged session, once it said who it is.
    target_session: Option<TargetSession>,
    /// The SQL panel: the statements the session ran, and their plans.
    sql: SqlPanel,
    /// Actions from outside the panel, for [`Self::poll`] to hand over.
    queued: Vec<DebugAction>,
}

/// After this long without the session connecting, the panel suggests
/// checking the setup.
const SLOW_CONNECT: Duration = Duration::from_secs(8);

/// What the debugger keeps across restarts of Pythia: breakpoints with
/// their state, conditions and hit counts, the watches, Break on
/// exceptions, Plan statistics and whether the SQL panel is shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub breakpoints: Vec<SavedBreakpoint>,
    pub watches: Vec<String>,
    pub break_on_exception: bool,
    pub plan_statistics: bool,
    pub sql_panel: bool,
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            breakpoints: Vec::new(),
            watches: Vec::new(),
            break_on_exception: false,
            plan_statistics: true,
            sql_panel: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedBreakpoint {
    pub breakpoint: Breakpoint,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub options: BreakpointOptions,
}

/// A value being changed in the Variables or Watches panel.
struct ValueEdit {
    /// The value's path, as a watch names it.
    path: String,
    draft: String,
    /// The field got the focus.
    focused: bool,
}

impl DebugSession {
    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            State::Waiting | State::Running | State::Suspended { .. }
        )
    }

    pub fn is_suspended(&self) -> bool {
        matches!(self.state, State::Suspended { .. })
    }

    /// Starts listening (on one of `ports`, any free port without) and asks
    /// the app to run `block` under the debugger. `host` is how the
    /// database reaches this machine; `entry` is a subprogram to stop in
    /// when the block calls it (unit and name).
    pub fn start(
        &mut self,
        block: String,
        host: String,
        ports: Option<&Ports>,
        entry: Option<(Unit, String)>,
        notify: impl Fn() + Send + 'static,
    ) -> Vec<DebugAction> {
        self.stop();
        self.stack.clear();
        self.frame = 0;
        self.variables.clear();
        self.watch_values.clear();
        self.exception = None;
        self.bound.clear();
        self.hits.clear();
        self.log.clear();
        // Units may have been compiled with debug information since.
        self.opaque_reasons.clear();
        self.opaque_asked.clear();
        self.block = block.clone();
        self.entry = entry;
        self.problem = None;
        self.target = None;
        self.waiting_since = None;
        self.target_session = None;
        self.sql.reset();
        match crate::setup::listen(ports).and_then(|l| Debugger::with_listener(l, notify)) {
            Ok(debugger) => {
                for bp in self.breakpoints.difference(&self.disabled) {
                    debugger.send(DebugCommand::SetBreakpoint(bp.clone()));
                }
                for (bp, options) in &self.options {
                    debugger.send(DebugCommand::SetBreakpointOptions(
                        bp.clone(),
                        options.clone(),
                    ));
                }
                if !self.watches.is_empty() {
                    debugger.send(DebugCommand::SetWatches(self.watches.clone()));
                }
                if self.break_on_exception {
                    debugger.send(DebugCommand::BreakOnException(true));
                }
                if let Some((unit, subprogram)) = &self.entry {
                    debugger.send(DebugCommand::BreakOnEntry {
                        unit: unit.clone(),
                        subprogram: subprogram.clone(),
                    });
                    self.log.push(format!(
                        "Stops on entry to {}",
                        subprogram_label(unit, subprogram)
                    ));
                }
                let port = debugger.port();
                self.debugger = Some(debugger);
                self.state = State::Waiting;
                self.target = Some((host.clone(), port));
                self.waiting_since = Some(Instant::now());
                self.log.push(format!(
                    "Waiting for the session to connect to {host}:{port}…"
                ));
                vec![DebugAction::RunTarget {
                    host,
                    port,
                    block,
                    statistics: self.sql.statistics,
                }]
            }
            Err(e) => {
                self.log.push(format!("Could not start the debugger: {e}"));
                self.state = State::Finished;
                Vec::new()
            }
        }
    }

    /// The debugged session ended without connecting: `CONNECT_TCP`
    /// failed with `error`. Stops waiting for it and says why; a setup
    /// problem asks for the debugger setup.
    pub fn target_failed(&mut self, error: &pythia_db::DbError) -> Vec<DebugAction> {
        if let Some(d) = self.debugger.take() {
            d.send(DebugCommand::Terminate);
        }
        self.state = State::Finished;
        self.waiting_since = None;
        let (host, port) = self.target.take().unwrap_or_default();
        match Problem::of(error) {
            Some(problem) => {
                let text = problem.explain(&host, port);
                self.log.push(text.clone());
                self.problem = Some(text.clone());
                vec![DebugAction::Setup { reason: Some(text) }]
            }
            None => {
                self.log
                    .push(format!("The session didn't connect: {}", error.text()));
                Vec::new()
            }
        }
    }

    /// Terminates the running call, if any.
    pub fn stop(&mut self) {
        if let Some(d) = self.debugger.take() {
            d.send(DebugCommand::Terminate);
        }
        if self.is_active() {
            self.state = State::Finished;
        }
    }

    pub fn command(&mut self, command: DebugCommand) {
        if let Some(d) = &self.debugger {
            if matches!(
                command,
                DebugCommand::Resume
                    | DebugCommand::RunTo(_)
                    | DebugCommand::StepOver
                    | DebugCommand::StepInto
                    | DebugCommand::StepOut
            ) {
                self.state = State::Running;
                self.exception = None;
            }
            d.send(command);
        }
    }

    /// Continues to a line while stopped, and stops there once (unless
    /// something else stops first).
    pub fn run_to(&mut self, unit: Unit, line: u32) {
        if !self.is_suspended() {
            self.log
                .push("Run to cursor goes on from a stop (a breakpoint, a step or Pause)".into());
            return;
        }
        self.command(DebugCommand::RunTo(Breakpoint { unit, line }));
    }

    /// Changes a value of the selected frame while stopped (`None` = NULL).
    pub fn set_value(&mut self, expression: &str, value: Option<String>) {
        if !self.is_suspended() {
            return;
        }
        self.value_error = None;
        self.command(DebugCommand::SetValue {
            expression: expression.to_owned(),
            value,
        });
    }

    /// Breakpoints, watches and Break on exceptions, to keep across
    /// restarts.
    pub fn saved(&self) -> Saved {
        Saved {
            breakpoints: self
                .breakpoints
                .iter()
                .map(|bp| SavedBreakpoint {
                    breakpoint: bp.clone(),
                    disabled: self.disabled.contains(bp),
                    options: self.options.get(bp).cloned().unwrap_or_default(),
                })
                .collect(),
            watches: self.watches.clone(),
            break_on_exception: self.break_on_exception,
            plan_statistics: self.sql.statistics,
            sql_panel: self.sql.show,
        }
    }

    /// Restores what [`Self::saved`] kept (before any debugging).
    pub fn restore(&mut self, saved: Saved) {
        if self.is_active() {
            return;
        }
        self.breakpoints.clear();
        self.disabled.clear();
        self.options.clear();
        for b in saved.breakpoints {
            if b.disabled {
                self.disabled.insert(b.breakpoint.clone());
            }
            if !b.options.is_empty() {
                self.options.insert(b.breakpoint.clone(), b.options);
            }
            self.breakpoints.insert(b.breakpoint);
        }
        self.watches = saved.watches;
        self.break_on_exception = saved.break_on_exception;
        self.sql.statistics = saved.plan_statistics;
        self.sql.show = saved.sql_panel;
    }

    /// The debugged session said who it is
    /// ([`crate::target::session_task`]): its SQL can be looked up.
    pub fn set_target_session(&mut self, session: TargetSession) {
        self.target_session = Some(session);
        if self.is_suspended() {
            self.sql
                .find(self.target_session.as_ref(), &mut self.queued);
        }
    }

    /// The statements the session ran, as looked up at stop `stop`
    /// ([`DebugAction::FindSql`]).
    pub fn sql_found(&mut self, stop: u64, result: Result<Vec<SessionSql>, String>) {
        let cx = sql_context(&self.state, &self.stack, self.frame, &self.variables);
        self.sql.found(
            stop,
            result,
            self.target_session.as_ref(),
            &cx,
            &mut self.queued,
        );
    }

    /// A statement's plan ([`DebugAction::LoadPlan`]).
    pub fn plan_loaded(&mut self, sql_id: String, child: i64, result: Result<CursorPlan, String>) {
        self.sql.plan_loaded(sql_id, child, result);
    }

    /// Shows or hides the SQL panel.
    pub fn show_sql(&mut self, show: bool) {
        self.sql.show = show;
        if show && self.is_suspended() {
            self.sql
                .find(self.target_session.as_ref(), &mut self.queued);
        }
    }

    pub fn sql_shown(&self) -> bool {
        self.sql.show
    }

    /// The statements the SQL panel lists.
    pub fn sql_statements(&self) -> &[SessionSql] {
        self.sql.statements()
    }

    /// The plan the SQL panel shows, once loaded.
    pub fn sql_plan(&self) -> Option<&CursorPlan> {
        self.sql.shown_plan()
    }

    /// Whether debug runs collect row source statistics for the SQL
    /// panel's plans (from the next start).
    pub fn set_plan_statistics(&mut self, on: bool) {
        self.sql.statistics = on;
    }

    pub fn plan_statistics(&self) -> bool {
        self.sql.statistics
    }

    /// Adds a breakpoint, enables a disabled one, or removes it.
    pub fn toggle_breakpoint(&mut self, unit: Unit, line: u32) {
        let bp = Breakpoint { unit, line };
        if self.disabled.contains(&bp) {
            self.set_enabled(&bp, true);
        } else if self.breakpoints.remove(&bp) {
            self.bound.remove(&bp);
            self.hits.remove(&bp);
            self.options.remove(&bp);
            self.command(DebugCommand::ClearBreakpoint(bp));
        } else {
            self.breakpoints.insert(bp.clone());
            self.command(DebugCommand::SetBreakpoint(bp));
        }
    }

    /// Enables or disables a breakpoint without forgetting it.
    pub fn set_enabled(&mut self, bp: &Breakpoint, enabled: bool) {
        if !self.breakpoints.contains(bp) {
            return;
        }
        if enabled {
            if self.disabled.remove(bp) {
                self.command(DebugCommand::SetBreakpoint(bp.clone()));
                if let Some(options) = self.options.get(bp).cloned() {
                    self.command(DebugCommand::SetBreakpointOptions(bp.clone(), options));
                }
            }
        } else if self.disabled.insert(bp.clone()) {
            self.bound.remove(bp);
            self.command(DebugCommand::ClearBreakpoint(bp.clone()));
        }
    }

    /// Sets a breakpoint's condition and hit count.
    pub fn set_options(&mut self, bp: &Breakpoint, options: BreakpointOptions) {
        if options.is_empty() {
            self.options.remove(bp);
        } else {
            self.options.insert(bp.clone(), options.clone());
        }
        self.command(DebugCommand::SetBreakpointOptions(bp.clone(), options));
    }

    pub fn add_watch(&mut self, expression: &str) {
        let expression = expression.trim();
        if expression.is_empty()
            || self
                .watches
                .iter()
                .any(|w| w.eq_ignore_ascii_case(expression))
        {
            return;
        }
        self.watches.push(expression.to_owned());
        self.command(DebugCommand::SetWatches(self.watches.clone()));
    }

    pub fn remove_watch(&mut self, index: usize) {
        if index < self.watches.len() {
            let removed = self.watches.remove(index);
            self.watch_values.retain(|w| w.expression != removed);
            self.command(DebugCommand::SetWatches(self.watches.clone()));
        }
    }

    pub fn set_break_on_exception(&mut self, on: bool) {
        self.break_on_exception = on;
        self.command(DebugCommand::BreakOnException(on));
    }

    /// The frame execution is shown at while suspended: the innermost one
    /// with source lines (code without debug information, such as SYS
    /// packages, has none).
    pub fn current_frame(&self) -> Option<&Frame> {
        if !self.is_suspended() {
            return None;
        }
        self.stack.get(self.execution_frame())
    }

    /// Index of [`Self::current_frame`] in the stack.
    fn execution_frame(&self) -> usize {
        self.stack
            .iter()
            .position(|f| f.location.line.is_some())
            .unwrap_or(0)
    }

    /// Selects a call stack frame while suspended: its variables are shown
    /// and watches and hover values come from it (the debugger reports
    /// them). Returns where to show its source.
    pub fn select_frame(&mut self, index: usize) -> Option<DebugAction> {
        let location = self.stack.get(index)?.location.clone();
        if self.is_suspended() && index != self.frame {
            self.frame = index;
            // Not the previous frame's values meanwhile.
            self.variables.clear();
            self.watch_values.clear();
            self.value_edit = None;
            self.value_error = None;
            self.command(DebugCommand::SelectFrame(index));
        }
        Some(DebugAction::ShowLocation(location))
    }

    /// Enabled breakpoint lines and the current execution line for a unit,
    /// for editor gutter markers.
    pub fn markers(&self, unit: &Unit) -> (Vec<u32>, Option<u32>) {
        let lines = self
            .breakpoints
            .iter()
            .filter(|b| &b.unit == unit && !self.disabled.contains(b))
            .map(|b| b.line)
            .collect();
        let current = self
            .current_frame()
            .filter(|f| &f.location.unit == unit)
            .and_then(|f| f.location.line);
        (lines, current)
    }

    /// The value of `expression` (a variable, `record.field`, …) while the
    /// selected frame is in `unit`, as hover text. Anonymous blocks match
    /// any block.
    pub fn hover(&self, unit: &Unit, expression: &str) -> Option<(String, String)> {
        if !self.is_suspended() {
            return None;
        }
        let selected = &self.stack.get(self.frame)?.location.unit;
        let same = selected == unit
            || (selected.unit_type == UnitType::Block && unit.unit_type == UnitType::Block);
        if !same {
            return None;
        }
        let path = condition::parse_path(expression)?;
        let var = condition::lookup(&self.variables, &path).or_else(|| {
            self.watch_values
                .iter()
                .find(|w| condition::parse_path(&w.expression).as_ref() == Some(&path))
                .and_then(|w| w.value.as_ref().ok())
        })?;
        let title = format!("{} {}", expression.trim(), var.type_name)
            .trim()
            .to_owned();
        let mut text = describe(var);
        if let Some(reason) = self
            .opaque_reasons
            .get(&(selected.clone(), var.name.clone()))
        {
            text = format!("{text}\n{reason}");
        }
        Some((title, text))
    }

    /// Why a variable of `unit` has no contents, if known.
    pub fn opaque_reason(&self, unit: &Unit, name: &str) -> Option<&str> {
        self.opaque_reasons
            .get(&(unit.clone(), name.to_owned()))
            .map(String::as_str)
    }

    /// Answers [`DebugAction::ExplainOpaque`].
    pub fn set_opaque_reasons(&mut self, unit: &Unit, reasons: Vec<(String, String)>) {
        for (name, reason) in reasons {
            self.opaque_reasons.insert((unit.clone(), name), reason);
        }
    }

    /// Asks why the selected frame's opaque variables have no contents,
    /// once per variable and session.
    fn explain_opaque(&mut self, actions: &mut Vec<DebugAction>) {
        let Some(frame) = self.stack.get(self.frame) else {
            return;
        };
        let unit = frame.location.unit.clone();
        let names: Vec<String> = self
            .variables
            .iter()
            .filter(|v| v.type_name == "OPAQUE")
            .map(|v| v.name.clone())
            .filter(|name| self.opaque_asked.insert((unit.clone(), name.clone())))
            .collect();
        if names.is_empty() {
            return;
        }
        let source = (unit.unit_type == UnitType::Block).then(|| self.block.clone());
        actions.push(DebugAction::ExplainOpaque {
            unit,
            line: frame.location.line,
            names,
            source,
        });
    }

    /// Drains debugger events; call every frame.
    pub fn poll(&mut self) -> Vec<DebugAction> {
        let mut actions = std::mem::take(&mut self.queued);
        let Some(debugger) = &self.debugger else {
            return actions;
        };
        let mut events = Vec::new();
        while let Some(e) = debugger.try_recv() {
            events.push(e);
        }
        for event in events {
            self.on_event(event, &mut actions);
        }
        actions
    }

    fn on_event(&mut self, event: DebugEvent, actions: &mut Vec<DebugAction>) {
        match event {
            DebugEvent::Listening { .. } => {}
            DebugEvent::Connected { version } => {
                self.state = State::Running;
                self.waiting_since = None;
                self.log.push(format!("Connected: {version}"));
            }
            DebugEvent::BreakpointBound {
                breakpoint,
                line,
                error,
            } => {
                let entry = match (line, error) {
                    (Some(l), _) => Ok(l),
                    (None, e) => Err(e.unwrap_or_else(|| "not set".into())),
                };
                if let Err(e) = &entry {
                    self.log.push(format!(
                        "Breakpoint at {} line {}: {e}",
                        breakpoint.unit, breakpoint.line
                    ));
                }
                self.bound.insert(breakpoint, entry);
            }
            DebugEvent::BreakpointHit { breakpoint, hits } => {
                self.hits.insert(breakpoint, hits);
            }
            DebugEvent::Suspended {
                reason,
                location,
                stack,
                frame,
                variables,
                watches,
                exception,
            } => {
                self.state = State::Suspended { reason };
                self.stack = stack;
                self.frame = frame;
                self.value_edit = None;
                self.value_error = None;
                self.variables = variables;
                self.watch_values = watches;
                if let Some(e) = &exception {
                    self.log.push(format!("Exception: {e}"));
                }
                self.exception = exception;
                let shown = self.current_frame().map(|f| f.location.clone());
                actions.push(DebugAction::ShowLocation(shown.unwrap_or(location)));
                self.explain_opaque(actions);
                self.sql.stopped(self.target_session.as_ref(), actions);
            }
            DebugEvent::Watches(values) => self.watch_values = values,
            DebugEvent::ValueSet { expression, error } => {
                if let Some(error) = error {
                    self.log.push(format!("Can't set {expression}: {error}"));
                    self.value_error = Some((expression, error));
                }
            }
            // Only the latest selection (a click may still be on its way).
            DebugEvent::FrameSelected {
                frame,
                variables,
                watches,
            } if frame == self.frame => {
                self.variables = variables;
                self.watch_values = watches;
                self.explain_opaque(actions);
                let cx = sql_context(&self.state, &self.stack, self.frame, &self.variables);
                self.sql.frame_selected(&cx, actions);
            }
            DebugEvent::FrameSelected { .. } => {}
            DebugEvent::SteppedOver(units) => {
                for unit in units {
                    self.log.push(format!(
                        "Step into went over {unit}: it was compiled without debug \
                         information (Settings → Debugger)"
                    ));
                }
            }
            DebugEvent::Resumed => {
                if !self.is_suspended() {
                    self.state = State::Running;
                }
            }
            DebugEvent::Terminated => {
                self.state = State::Finished;
                self.debugger = None;
                self.waiting_since = None;
                self.stack.clear();
                self.frame = 0;
                self.exception = None;
                self.value_edit = None;
                self.value_error = None;
                self.log.push("Debug session ended".into());
                actions.push(DebugAction::Finished);
            }
            DebugEvent::Error(e) => self.log.push(e),
        }
    }

    /// Debugger shortcuts; call before the editors see the input.
    pub fn handle_keys(&mut self, ctx: &egui::Context) {
        if !self.is_active() {
            return;
        }
        let pressed = |s: &KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(s));
        // Shifted variants first so F7 doesn't swallow Shift+F7.
        if pressed(&STEP_OUT) {
            self.command(DebugCommand::StepOut);
        } else if pressed(&STOP) {
            self.stop();
        } else if pressed(&CONTINUE) {
            self.command(DebugCommand::Resume);
        } else if pressed(&STEP_OVER) {
            self.command(DebugCommand::StepOver);
        } else if pressed(&STEP_INTO) {
            self.command(DebugCommand::StepInto);
        }
    }

    /// The debugger panel. Returns actions (e.g. jumping to a stack frame).
    pub fn ui(&mut self, ui: &mut Ui) -> Vec<DebugAction> {
        let mut actions = Vec::new();
        actions.extend(self.toolbar(ui));
        actions.extend(self.connect_hint(ui));
        ui.separator();
        if self.sql.show {
            let cx = sql_context(&self.state, &self.stack, self.frame, &self.variables);
            let cx = sql_panel::Context {
                active: self.is_active(),
                ..cx
            };
            let width = ui.available_width();
            let panel = egui::Panel::right("dbg-sql")
                .resizable(true)
                .default_size(width * 0.55)
                .size_range(240.0..=width * 0.85)
                .show(ui, |ui| self.sql.ui(ui, &cx, &mut actions));
            // Columns don't keep clear of panels: the rest is theirs.
            let mut rest = ui.available_rect_before_wrap();
            rest.max.x = rest
                .max
                .x
                .min(panel.response.rect.min.x - ui.spacing().item_spacing.x);
            let mut columns = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rest)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            self.columns_ui(&mut columns, &mut actions);
        } else {
            self.columns_ui(ui, &mut actions);
        }
        actions
    }

    /// Call stack, variables, watches and breakpoints side by side.
    fn columns_ui(&mut self, ui: &mut Ui, actions: &mut Vec<DebugAction>) {
        ui.columns(4, |cols| {
            // Long names and values stay in their column.
            for col in cols.iter_mut() {
                let clip = col.max_rect().intersect(col.clip_rect());
                col.set_clip_rect(clip);
            }
            cols[0].strong("Call stack");
            let mut select = None;
            egui::ScrollArea::vertical()
                .id_salt("dbg-stack")
                .show(&mut cols[0], |ui| {
                    for (i, frame) in self.stack.iter().enumerate() {
                        let text = RichText::new(frame_label(frame)).monospace();
                        if ui.selectable_label(i == self.frame, text).clicked() {
                            select = Some(i);
                        }
                    }
                    if self.stack.is_empty() {
                        ui.weak("—");
                    }
                    if !self.log.is_empty() {
                        ui.add_space(6.0);
                        // Not justified: that would stretch the words.
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            for line in self.log.iter().rev().take(5) {
                                ui.add(
                                    egui::Label::new(RichText::new(line).small().weak()).truncate(),
                                )
                                .on_hover_text(line);
                            }
                        });
                    }
                });
            if let Some(action) = select.and_then(|i| self.select_frame(i)) {
                actions.push(action);
            }

            cols[1].horizontal(|ui| {
                ui.strong("Variables")
                    .on_hover_text("Double-click a value to change it while stopped");
                // Say whose, when it isn't where execution stopped.
                if self.is_suspended()
                    && self.frame != self.execution_frame()
                    && let Some(frame) = self.stack.get(self.frame)
                {
                    ui.add(
                        egui::Label::new(RichText::new(frame_label(frame)).monospace().weak())
                            .truncate(),
                    );
                }
            });
            let unit = self.stack.get(self.frame).map(|f| &f.location.unit);
            let mut rows = Rows {
                editable: self.is_suspended(),
                edit: &mut self.value_edit,
                error: self.value_error.as_ref(),
                action: None,
            };
            egui::ScrollArea::vertical()
                .id_salt("dbg-vars")
                .show(&mut cols[1], |ui| {
                    for (i, var) in self.variables.iter().enumerate() {
                        let reason = unit
                            .and_then(|u| self.opaque_reasons.get(&(u.clone(), var.name.clone())))
                            .map(String::as_str);
                        variable_ui(ui, var, "", i, &mut rows, reason);
                    }
                    if self.variables.is_empty() {
                        ui.weak("—");
                    }
                });
            let action = rows.action;
            self.row_action(action);

            self.watches_ui(&mut cols[2]);
            self.breakpoints_ui(&mut cols[3]);
        });
    }

    fn toolbar(&mut self, ui: &mut Ui) -> Option<DebugAction> {
        let suspended = self.is_suspended();
        let active = self.is_active();
        let mut setup = false;
        ui.horizontal(|ui| {
            let button = |ui: &mut Ui, label: &str, shortcut: &KeyboardShortcut, enabled: bool| {
                let text = format!("{label}  {}", ui.ctx().format_shortcut(shortcut));
                ui.add_enabled(enabled, egui::Button::new(text)).clicked()
            };
            if button(ui, "Continue", &CONTINUE, suspended) {
                self.command(DebugCommand::Resume);
            }
            if button(ui, "Step over", &STEP_OVER, suspended) {
                self.command(DebugCommand::StepOver);
            }
            if button(ui, "Step into", &STEP_INTO, suspended) {
                self.command(DebugCommand::StepInto);
            }
            if button(ui, "Step out", &STEP_OUT, suspended) {
                self.command(DebugCommand::StepOut);
            }
            if ui
                .add_enabled(active && !suspended, egui::Button::new("Pause"))
                .clicked()
            {
                self.command(DebugCommand::Pause);
            }
            if button(ui, "Stop", &STOP, active) {
                self.stop();
            }
            ui.separator();
            let mut on = self.break_on_exception;
            if ui
                .checkbox(&mut on, "Break on exceptions")
                .on_hover_text("Stop where an exception is raised, handled or not")
                .changed()
            {
                self.set_break_on_exception(on);
            }
            ui.checkbox(&mut self.sql.statistics, "Plan statistics")
                .on_hover_text(
                    "Collect row source statistics in the debugged session (STATISTICS_LEVEL = \
                     ALL), so the plans of the statements it runs show actual rows next to the \
                     estimates. Costs some time per row; applies from the next start.",
                );
            let mut show = self.sql.show;
            if ui
                .toggle_value(&mut show, "SQL")
                .on_hover_text("The statements the session ran, by line, with their plans")
                .changed()
            {
                self.show_sql(show);
            }
            ui.separator();
            let (text, color) = match &self.state {
                State::Idle => ("Not debugging".to_owned(), ui.visuals().weak_text_color()),
                State::Waiting => (
                    "Waiting for the session…".to_owned(),
                    ui.visuals().warn_fg_color,
                ),
                State::Running => ("Running".to_owned(), Color32::from_rgb(0x3f, 0xb9, 0x50)),
                State::Suspended { reason } => {
                    let at = self.current_frame().map(frame_label).unwrap_or_default();
                    let text = match reason {
                        StopReason::Breakpoint => format!("Stopped at breakpoint, {at}"),
                        StopReason::Step => format!("Stopped after step, {at}"),
                        StopReason::Pause => format!("Paused at {at}"),
                        StopReason::Entry => format!("Stopped on entry, {at}"),
                        StopReason::Cursor => format!("Stopped at cursor, {at}"),
                        StopReason::Exception => format!(
                            "Exception at {at}: {}",
                            self.exception.as_deref().unwrap_or("raised")
                        ),
                    };
                    let color = if *reason == StopReason::Exception {
                        ui.visuals().error_fg_color
                    } else {
                        ui.visuals().warn_fg_color
                    };
                    (text, color)
                }
                State::Finished => ("Finished".to_owned(), ui.visuals().weak_text_color()),
            };
            // Right to left, so the state text can take the rest.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                setup = ui
                    .button("Setup…")
                    .on_hover_text(
                        "Check what the database needs to connect to the debugger, and fix it",
                    )
                    .clicked();
                ui.add(egui::Label::new(RichText::new(text).color(color)).truncate());
            });
        });
        setup.then_some(DebugAction::Setup { reason: None })
    }

    /// Below the toolbar: why the last start didn't connect, or that the
    /// session is slow to.
    fn connect_hint(&mut self, ui: &mut Ui) -> Option<DebugAction> {
        let slow = match (&self.state, self.waiting_since) {
            (State::Waiting, Some(since)) => {
                let waited = since.elapsed();
                if waited < SLOW_CONNECT {
                    ui.ctx().request_repaint_after(SLOW_CONNECT - waited);
                    return None;
                }
                let (host, port) = self.target.clone().unwrap_or_default();
                Some(format!(
                    "The database hasn't connected to {host}:{port} yet: a firewall may be \
                     dropping the connection, or {host} isn't how the database reaches this \
                     computer."
                ))
            }
            _ => None,
        };
        let (text, color, button) = match (slow, &self.problem) {
            (Some(text), _) => (text, ui.visuals().warn_fg_color, "Check setup…"),
            (None, Some(problem)) if self.state == State::Finished => {
                (problem.clone(), ui.visuals().error_fg_color, "Fix…")
            }
            _ => return None,
        };
        let mut clicked = false;
        ui.horizontal(|ui| {
            clicked = ui.button(button).clicked();
            ui.add(egui::Label::new(RichText::new(&text).color(color)).wrap());
        });
        clicked.then_some(DebugAction::Setup { reason: Some(text) })
    }

    fn row_action(&mut self, action: Option<RowAction>) {
        match action {
            Some(RowAction::Watch(expression)) => self.add_watch(&expression),
            Some(RowAction::Set(path, value)) => self.set_value(&path, value),
            None => {}
        }
    }

    fn watches_ui(&mut self, ui: &mut Ui) {
        ui.strong("Watches");
        let mut remove = None;
        let stale = !self.is_suspended();
        let mut rows = Rows {
            editable: !stale,
            edit: &mut self.value_edit,
            error: self.value_error.as_ref(),
            action: None,
        };
        egui::ScrollArea::vertical()
            .id_salt("dbg-watches")
            .max_height((ui.available_height() - 28.0).max(40.0))
            .show(ui, |ui| {
                for (i, expression) in self.watches.iter().enumerate() {
                    let value = self
                        .watch_values
                        .iter()
                        .find(|w| &w.expression == expression);
                    ui.horizontal(|ui| {
                        if ui.small_button("×").on_hover_text("Remove watch").clicked() {
                            remove = Some(i);
                        }
                        match value.map(|w| &w.value) {
                            Some(Ok(var)) if var.children.is_empty() => {
                                ui.label(RichText::new(expression).monospace().strong());
                                if let Some(response) =
                                    value_ui(ui, expression, var, &mut rows, stale, true)
                                {
                                    if rows.can_edit(var) && response.double_clicked() {
                                        rows.start(expression, var);
                                    }
                                    response.context_menu(|ui| {
                                        value_menu(ui, expression, var, &mut rows, false)
                                    });
                                }
                            }
                            Some(Ok(var)) => {
                                let mut named = var.clone();
                                named.name = expression.clone();
                                ui.vertical(|ui| {
                                    variable_ui(ui, &named, "", 1000 + i, &mut rows, None);
                                });
                            }
                            Some(Err(e)) => {
                                ui.label(RichText::new(expression).monospace().strong());
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(e).small().color(ui.visuals().warn_fg_color),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(e);
                            }
                            None => {
                                ui.label(RichText::new(expression).monospace().strong());
                                ui.weak("—");
                            }
                        }
                    });
                    if let Some((_, error)) = rows.error.filter(|(path, _)| path == expression) {
                        error_ui(ui, error);
                    }
                }
                if self.watches.is_empty() {
                    ui.weak("Right-click a variable or type an expression below");
                }
            });
        let action = rows.action;
        self.row_action(action);
        if let Some(i) = remove {
            self.remove_watch(i);
        }
        let response = ui.add(
            egui::TextEdit::singleline(&mut self.new_watch)
                .hint_text("Add watch: l_rec.name, pkg.g_var")
                .desired_width(f32::INFINITY),
        );
        if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            let expression = std::mem::take(&mut self.new_watch);
            self.add_watch(&expression);
            response.request_focus();
        }
    }

    fn breakpoints_ui(&mut self, ui: &mut Ui) {
        ui.strong("Breakpoints");
        let mut remove = None;
        let mut enable = None;
        let mut edit = None;
        let mut apply = None;
        egui::ScrollArea::vertical()
            .id_salt("dbg-bps")
            .show(ui, |ui| {
                for bp in &self.breakpoints {
                    let enabled = !self.disabled.contains(bp);
                    ui.horizontal(|ui| {
                        let mut on = enabled;
                        if ui.checkbox(&mut on, "").on_hover_text("Enabled").changed() {
                            enable = Some((bp.clone(), on));
                        }
                        let label = match self.bound.get(bp) {
                            Some(Ok(line)) if *line != bp.line => {
                                format!("{} : {} → {line}", unit_label(&bp.unit), bp.line)
                            }
                            _ => format!("{} : {}", unit_label(&bp.unit), bp.line),
                        };
                        let mut text = RichText::new(label).monospace();
                        if !enabled {
                            text = text.color(ui.visuals().weak_text_color());
                        }
                        let response = if let Some(Err(e)) = self.bound.get(bp) {
                            ui.label(text.color(ui.visuals().warn_fg_color))
                                .on_hover_text(e)
                        } else {
                            ui.label(text)
                        };
                        if let Some(hits) = self.hits.get(bp) {
                            ui.weak(format!("{hits}×"))
                                .on_hover_text("Hits in this session");
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("×").on_hover_text("Remove").clicked() {
                                remove = Some(bp.clone());
                            }
                            if ui
                                .small_button("if…")
                                .on_hover_text("Condition and hit count")
                                .clicked()
                            {
                                edit = Some(bp.clone());
                            }
                        });
                        response.context_menu(|ui| {
                            if ui.button("Condition…").clicked() {
                                edit = Some(bp.clone());
                                ui.close();
                            }
                            if ui
                                .button(if enabled { "Disable" } else { "Enable" })
                                .clicked()
                            {
                                enable = Some((bp.clone(), !enabled));
                                ui.close();
                            }
                            if ui.button("Remove").clicked() {
                                remove = Some(bp.clone());
                                ui.close();
                            }
                        });
                    });
                    let editing = self.editing.as_mut().filter(|(b, ..)| b == bp);
                    if let Some((_, condition, hit_count)) = editing {
                        ui.indent(("bp-edit", bp.line, &bp.unit.name), |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(condition)
                                    .hint_text("Condition, e.g. l_total > 3")
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY),
                            );
                            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                            ui.horizontal(|ui| {
                                ui.label("Stop from hit");
                                ui.add(egui::DragValue::new(hit_count).range(0..=100_000))
                                    .on_hover_text("0 or 1: every hit");
                                if ui.button("Apply").clicked() || enter {
                                    let condition = condition.trim();
                                    apply = Some((
                                        bp.clone(),
                                        BreakpointOptions {
                                            condition: (!condition.is_empty())
                                                .then(|| condition.to_owned()),
                                            hit_count: (*hit_count > 1).then_some(*hit_count),
                                        },
                                    ));
                                }
                                if ui.button("Cancel").clicked() {
                                    edit = Some(bp.clone());
                                }
                            });
                        });
                    } else if let Some(options) = self.options.get(bp) {
                        let mut parts = Vec::new();
                        if let Some(c) = &options.condition {
                            parts.push(format!("if {c}"));
                        }
                        if let Some(n) = options.hit_count {
                            parts.push(format!("from hit {n}"));
                        }
                        ui.indent(("bp-opts", bp.line, &bp.unit.name), |ui| {
                            let text = parts.join(" · ");
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&text)
                                        .monospace()
                                        .color(ui.visuals().hyperlink_color),
                                )
                                .truncate(),
                            )
                            .on_hover_text(text);
                        });
                    }
                }
                if self.breakpoints.is_empty() {
                    ui.weak("Click the editor gutter or press Ctrl+B");
                }
            });
        if let Some((bp, on)) = enable {
            self.set_enabled(&bp, on);
        }
        if let Some(bp) = remove {
            self.editing = None;
            self.disabled.remove(&bp);
            self.toggle_breakpoint(bp.unit, bp.line);
        }
        if let Some((bp, options)) = apply {
            self.editing = None;
            self.set_options(&bp, options);
        } else if let Some(bp) = edit {
            self.editing = match self.editing.take() {
                Some((b, ..)) if b == bp => None,
                _ => {
                    let options = self.options.get(&bp).cloned().unwrap_or_default();
                    Some((
                        bp,
                        options.condition.unwrap_or_default(),
                        options.hit_count.unwrap_or(0),
                    ))
                }
            };
        }
    }
}

fn unit_label(unit: &Unit) -> String {
    match unit.unit_type {
        UnitType::Block => "anonymous block".into(),
        _ => unit.name.to_lowercase(),
    }
}

/// A subprogram of a unit, e.g. `orders_api.load_order` (a standalone
/// procedure or function is just its name).
fn subprogram_label(unit: &Unit, subprogram: &str) -> String {
    match unit.unit_type {
        UnitType::Package | UnitType::PackageBody | UnitType::Type | UnitType::TypeBody => {
            format!("{}.{}", unit_label(unit), subprogram.to_lowercase())
        }
        _ => unit_label(unit),
    }
}

/// `unit:line`, with the subprogram for packages and types, e.g.
/// `orders_api.load_order:31`.
fn frame_label(frame: &Frame) -> String {
    let unit = &frame.location.unit;
    let line = frame
        .location
        .line
        .map(|l| format!(":{l}"))
        .unwrap_or_default();
    let subprogram = match frame.method.as_str() {
        // The package's initialization section (its BEGIN … END).
        "<clinit>" => Some("(initialization)".to_owned()),
        // Generated names, e.g. in code without debug information.
        m if m.starts_with('<') || m.is_empty() => None,
        m => Some(m.to_lowercase()),
    };
    match (unit.unit_type, subprogram) {
        (
            UnitType::Package | UnitType::PackageBody | UnitType::Type | UnitType::TypeBody,
            Some(subprogram),
        ) => format!("{}.{subprogram}{line}", unit_label(unit)),
        _ => format!("{}{line}", unit_label(unit)),
    }
}

fn value_text(ui: &Ui, var: &Variable, stale: bool) -> RichText {
    let text = match &var.value {
        Some(v) => RichText::new(v).monospace(),
        None => RichText::new("NULL")
            .monospace()
            .italics()
            .color(ui.visuals().weak_text_color()),
    };
    if stale {
        text.color(ui.visuals().weak_text_color())
    } else {
        text
    }
}

/// What the variable rows of the Variables and Watches panels share while
/// they are drawn.
struct Rows<'a> {
    /// Values can be changed (stopped).
    editable: bool,
    edit: &'a mut Option<ValueEdit>,
    /// Why the last change failed: its path and the reason.
    error: Option<&'a (String, String)>,
    /// What a row asked for.
    action: Option<RowAction>,
}

enum RowAction {
    Watch(String),
    /// Change the value at a path (`None` = NULL).
    Set(String, Option<String>),
}

impl Rows<'_> {
    fn can_edit(&self, var: &Variable) -> bool {
        self.editable && var.children.is_empty() && value::changeable(&var.type_name)
    }

    /// Starts changing a value: a field with its text.
    fn start(&mut self, path: &str, var: &Variable) {
        *self.edit = Some(ValueEdit {
            path: path.to_owned(),
            draft: var.value.clone().unwrap_or_default(),
            focused: false,
        });
    }
}

/// A variable's name as shown: lowercase, but text keys (`('Key')`) and
/// watch expressions (`l_map('Key')`) as they are.
fn shown_name(name: &str) -> String {
    if name.contains('(') {
        name.to_owned()
    } else {
        name.to_lowercase()
    }
}

/// The path of a variable, field or element below `parent`, as a watch
/// names it: `l_rec.name`, `l_tab(3)`, `l_map('Key')`.
fn child_path(parent: &str, name: &str) -> String {
    let name = shown_name(name);
    if parent.is_empty() {
        name
    } else if name.starts_with('(') {
        format!("{parent}{name}")
    } else {
        format!("{parent}.{name}")
    }
}

/// A variable and its fields/elements; right-click offers "Watch" with the
/// variable's full path (`parent` is the path of the enclosing one) and,
/// while stopped, changing a scalar's value (also on double-click).
/// `opaque` says why it has no contents, if known.
fn variable_ui(
    ui: &mut Ui,
    var: &Variable,
    parent: &str,
    index: usize,
    rows: &mut Rows,
    opaque: Option<&str>,
) {
    let name = shown_name(&var.name);
    let path = child_path(parent, &var.name);
    if var.children.is_empty() {
        let response = ui
            .horizontal(|ui| {
                ui.label(RichText::new(&name).monospace().strong());
                value_ui(ui, &path, var, rows, false, false);
                ui.weak(&var.type_name);
            })
            .response
            .interact(egui::Sense::click());
        if rows.can_edit(var) && response.double_clicked() {
            rows.start(&path, var);
        }
        response.context_menu(|ui| value_menu(ui, &path, var, rows, true));
        if let Some((_, error)) = rows.error.filter(|(p, _)| *p == path) {
            error_ui(ui, error);
        }
        if let Some(reason) = opaque {
            ui.label(
                RichText::new(reason)
                    .small()
                    .color(ui.visuals().warn_fg_color),
            )
            .on_hover_text(format!(
                "{reason}, so the debugger can't read its contents. Compile that package \
                     with debug information (open it in a program window with Settings → \
                     Debugger → Compile with debug information) and debug again."
            ));
        }
    } else {
        let summary = var
            .value
            .as_deref()
            .map(|v| format!("  {v}"))
            .unwrap_or_default();
        let header = format!("{name}  {}{summary}", var.type_name);
        let response = egui::CollapsingHeader::new(RichText::new(header).monospace())
            .id_salt(("var", index, &var.name, parent))
            .show(ui, |ui| {
                for (i, child) in var.children.iter().enumerate() {
                    variable_ui(ui, child, &path, i, rows, None);
                }
            });
        response
            .header_response
            .context_menu(|ui| value_menu(ui, &path, var, rows, true));
    }
}

/// A value, or the field that changes it. `sense`: the label takes clicks
/// (for a context menu and double-click).
fn value_ui(
    ui: &mut Ui,
    path: &str,
    var: &Variable,
    rows: &mut Rows,
    stale: bool,
    sense: bool,
) -> Option<egui::Response> {
    if let Some(edit) = rows.edit.as_mut().filter(|e| e.path == path) {
        let response = ui
            .add(
                egui::TextEdit::singleline(&mut edit.draft)
                    .font(egui::TextStyle::Monospace)
                    .desired_width((ui.available_width() - 60.0).max(80.0)),
            )
            .on_hover_text("Enter sets the value (empty for NULL), Esc cancels");
        if !edit.focused {
            response.request_focus();
            edit.focused = true;
        }
        if response.lost_focus() {
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                let value = Some(edit.draft.clone());
                rows.action = Some(RowAction::Set(path.to_owned(), value));
            }
            *rows.edit = None;
        }
        return None;
    }
    let label = egui::Label::new(value_text(ui, var, stale));
    Some(if sense {
        ui.add(label.sense(egui::Sense::click()))
    } else {
        ui.add(label)
    })
}

/// The context menu of a variable or watch: watch it, change it, copy it.
fn value_menu(ui: &mut Ui, path: &str, var: &Variable, rows: &mut Rows, watch: bool) {
    if watch && ui.button(format!("Watch {path}")).clicked() {
        rows.action = Some(RowAction::Watch(path.to_owned()));
        ui.close();
    }
    if rows.can_edit(var) {
        if ui.button("Set value…").clicked() {
            rows.start(path, var);
            ui.close();
        }
        if var.value.is_some() && ui.button("Set to NULL").clicked() {
            rows.action = Some(RowAction::Set(path.to_owned(), None));
            ui.close();
        }
    }
    if let Some(v) = &var.value
        && ui.button("Copy value").clicked()
    {
        ui.ctx().copy_text(v.clone());
        ui.close();
    }
}

fn error_ui(ui: &mut Ui, error: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(error)
                .small()
                .color(ui.visuals().error_fg_color),
        )
        .wrap(),
    );
}

/// Where the SQL panel's statements are: the selected frame, the block
/// being debugged (the outermost frame) and the frame's variables.
fn sql_context<'a>(
    state: &State,
    stack: &'a [Frame],
    frame: usize,
    variables: &[Variable],
) -> sql_panel::Context<'a> {
    let suspended = matches!(state, State::Suspended { .. });
    let at = stack
        .get(frame)
        .and_then(|f| Some((&f.location.unit, f.location.line?)))
        .filter(|_| suspended);
    let block = stack
        .last()
        .map(|f| &f.location.unit)
        .filter(|u| u.unit_type == UnitType::Block);
    const MAX: usize = 40;
    let mut shown: Vec<String> = variables
        .iter()
        .take(MAX)
        .map(|v| {
            let name = v.name.to_lowercase();
            if v.children.is_empty() {
                format!("{name} = {}", v.value.as_deref().unwrap_or("NULL"))
            } else {
                format!(
                    "{name} ({}) = {}",
                    v.type_name,
                    describe(v).replace('\n', "; ")
                )
            }
        })
        .collect();
    if variables.len() > MAX {
        shown.push(format!("… {} more", variables.len() - MAX));
    }
    sql_panel::Context {
        suspended,
        active: suspended,
        at,
        block,
        variables: shown,
    }
}

/// Hover text for a variable: its value, or its fields/elements.
fn describe(var: &Variable) -> String {
    const MAX: usize = 12;
    let value = |v: &Variable| v.value.clone().unwrap_or_else(|| "NULL".into());
    if var.children.is_empty() {
        return value(var);
    }
    let mut lines: Vec<String> = var.value.iter().cloned().collect();
    for child in var.children.iter().take(MAX) {
        let shown = if child.children.is_empty() {
            value(child)
        } else {
            format!("{} …", child.type_name)
        };
        lines.push(format!("{} = {shown}", child.name.to_lowercase()));
    }
    if var.children.len() > MAX {
        lines.push(format!("… {} more", var.children.len() - MAX));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, value: Option<&str>, children: Vec<Variable>) -> Variable {
        Variable {
            name: name.into(),
            type_name: "NUMBER".into(),
            value: value.map(Into::into),
            children,
        }
    }

    #[test]
    fn a_start_that_cannot_connect_ends_and_says_why() {
        let mut s = DebugSession::default();
        let actions = s.start(
            "begin null; end;".into(),
            "devbox".into(),
            None,
            None,
            || {},
        );
        let Some(DebugAction::RunTarget { port, .. }) = actions.first().cloned() else {
            panic!("{actions:?}")
        };
        assert_eq!(s.state, State::Waiting);
        let error = pythia_db::DbError::new(
            "ORA-24247: network access denied by access control list (ACL)\n\
             ORA-06512: at \"SYS.DBMS_DEBUG_JDWP\", line 68",
        );
        let actions = s.target_failed(&error);
        assert_eq!(s.state, State::Finished);
        assert!(!s.is_active());
        let reason = format!(
            "No network ACL entry lets the database connect to devbox port {port} for \
             debugging (JDWP)."
        );
        assert_eq!(s.problem.as_deref(), Some(reason.as_str()));
        assert_eq!(
            actions,
            [DebugAction::Setup {
                reason: Some(reason)
            }]
        );
        // The next start forgets it; an error that isn't about the setup
        // is only logged.
        s.start(
            "begin null; end;".into(),
            "devbox".into(),
            None,
            None,
            || {},
        );
        assert_eq!(s.problem, None);
        let actions = s.target_failed(&pythia_db::DbError::new("ORA-03113: end-of-file"));
        assert!(actions.is_empty());
        assert_eq!(
            s.log.last().map(String::as_str),
            Some("The session didn't connect: ORA-03113: end-of-file")
        );
    }

    #[test]
    fn markers_follow_breakpoints() {
        let mut s = DebugSession::default();
        let unit = Unit::new("PYTHIA", "P", UnitType::Procedure);
        s.toggle_breakpoint(unit.clone(), 5);
        s.toggle_breakpoint(unit.clone(), 9);
        s.toggle_breakpoint(unit.clone(), 5);
        assert_eq!(s.markers(&unit), (vec![9], None));
        s.state = State::Suspended {
            reason: StopReason::Breakpoint,
        };
        s.stack = vec![Frame {
            location: Location {
                unit: unit.clone(),
                line: Some(9),
            },
            method: "P".into(),
        }];
        assert_eq!(s.markers(&unit).1, Some(9));
    }

    fn frame(unit: &Unit, method: &str, line: Option<u32>) -> Frame {
        Frame {
            location: Location {
                unit: unit.clone(),
                line,
            },
            method: method.into(),
        }
    }

    #[test]
    fn execution_shown_in_the_nearest_frame_with_source() {
        // Paused inside code without debug information.
        let sys = Unit::new("SYS", "DBMS_SESSION", UnitType::PackageBody);
        let api = Unit::new("PYTHIA", "API", UnitType::PackageBody);
        let mut s = DebugSession {
            state: State::Suspended {
                reason: StopReason::Pause,
            },
            stack: vec![
                frame(&sys, "<procedure$29>", None),
                frame(&api, "SPIN", Some(69)),
            ],
            ..Default::default()
        };
        assert_eq!(s.current_frame().map(|f| f.method.as_str()), Some("SPIN"));
        assert_eq!(s.markers(&api).1, Some(69));
        assert_eq!(s.markers(&sys).1, None);
        s.state = State::Running;
        assert!(s.current_frame().is_none());
        assert_eq!(s.markers(&api).1, None);
    }

    #[test]
    fn frame_labels_name_the_subprogram() {
        let body = Unit::new("PYTHIA", "ORDERS_API", UnitType::PackageBody);
        let proc = Unit::new("PYTHIA", "DBG_DEMO", UnitType::Procedure);
        let block = Unit::new("PYTHIA", "B1", UnitType::Block);
        let sys = Unit::new("SYS", "DBMS_SESSION", UnitType::PackageBody);
        assert_eq!(
            frame_label(&frame(&body, "LOAD_ORDER", Some(31))),
            "orders_api.load_order:31"
        );
        assert_eq!(
            frame_label(&frame(&body, "<clinit>", Some(40))),
            "orders_api.(initialization):40"
        );
        assert_eq!(
            frame_label(&frame(&proc, "DBG_DEMO", Some(3))),
            "dbg_demo:3"
        );
        assert_eq!(
            frame_label(&frame(&block, "BLOCK", Some(4))),
            "anonymous block:4"
        );
        assert_eq!(
            frame_label(&frame(&sys, "<procedure$29>", None)),
            "dbms_session"
        );
    }

    #[test]
    fn disabled_breakpoints_stay_listed() {
        let mut s = DebugSession::default();
        let unit = Unit::new("PYTHIA", "P", UnitType::Procedure);
        s.toggle_breakpoint(unit.clone(), 5);
        let bp = Breakpoint {
            unit: unit.clone(),
            line: 5,
        };
        s.set_enabled(&bp, false);
        assert_eq!(s.markers(&unit).0, Vec::<u32>::new());
        assert!(s.breakpoints.contains(&bp));
        // Toggling a disabled breakpoint enables it again.
        s.toggle_breakpoint(unit.clone(), 5);
        assert_eq!(s.markers(&unit).0, vec![5]);
        s.set_options(
            &bp,
            BreakpointOptions {
                condition: Some("x > 1".into()),
                hit_count: None,
            },
        );
        assert!(s.options.contains_key(&bp));
        s.toggle_breakpoint(unit, 5);
        assert!(s.breakpoints.is_empty() && s.options.is_empty());
    }

    #[test]
    fn hover_values_while_stopped() {
        let mut s = DebugSession::default();
        let unit = Unit::new("PYTHIA", "P", UnitType::Procedure);
        s.stack = vec![Frame {
            location: Location {
                unit: unit.clone(),
                line: Some(3),
            },
            method: "P".into(),
        }];
        s.variables = vec![
            var("L_TOTAL", Some("6"), vec![]),
            var("L_REC", None, vec![var("A", Some("7"), vec![])]),
        ];
        s.watch_values = vec![Watch {
            expression: "pkg.g_count".into(),
            value: Ok(var("G_COUNT", Some("2"), vec![])),
        }];
        assert_eq!(s.hover(&unit, "l_total"), None, "only while suspended");
        s.state = State::Suspended {
            reason: StopReason::Step,
        };
        assert_eq!(s.hover(&unit, "L_Total").unwrap().1, "6");
        assert_eq!(s.hover(&unit, "l_rec.a").unwrap().1, "7");
        assert_eq!(s.hover(&unit, "l_rec").unwrap().1, "a = 7");
        assert_eq!(s.hover(&unit, "PKG.G_COUNT").unwrap().1, "2");
        assert_eq!(s.hover(&unit, "l_other"), None);
        let other = Unit::new("PYTHIA", "Q", UnitType::Procedure);
        assert_eq!(s.hover(&other, "l_total"), None, "other unit");
    }

    /// Opaque variables are explained once per session, for the frame they
    /// belong to; the reason shows with the value.
    #[test]
    fn opaque_variables_are_explained() {
        let api = Unit::new("PYTHIA", "API", UnitType::PackageBody);
        let mut s = DebugSession::default();
        let opaque = Variable {
            type_name: "OPAQUE".into(),
            ..var("L_ORDER", Some("(contents not available)"), vec![])
        };
        let stop = |line| DebugEvent::Suspended {
            reason: StopReason::Breakpoint,
            location: Location {
                unit: api.clone(),
                line: Some(line),
            },
            stack: vec![frame(&api, "PROCESS", Some(line))],
            frame: 0,
            variables: vec![opaque.clone(), var("L_TOTAL", Some("0"), vec![])],
            watches: Vec::new(),
            exception: None,
        };
        let mut actions = Vec::new();
        s.on_event(stop(41), &mut actions);
        assert_eq!(
            actions.last(),
            Some(&DebugAction::ExplainOpaque {
                unit: api.clone(),
                line: Some(41),
                names: vec!["L_ORDER".into()],
                source: None,
            })
        );
        s.set_opaque_reasons(
            &api,
            vec![("L_ORDER".into(), "T_ORDER is declared in TYPES, …".into())],
        );
        assert_eq!(
            s.opaque_reason(&api, "L_ORDER"),
            Some("T_ORDER is declared in TYPES, …")
        );
        assert!(
            s.hover(&api, "l_order")
                .unwrap()
                .1
                .ends_with("declared in TYPES, …")
        );
        // The next stop doesn't ask again.
        let mut actions = Vec::new();
        s.on_event(stop(42), &mut actions);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, DebugAction::ExplainOpaque { .. }))
        );
    }

    #[test]
    fn selected_frame_has_the_variables() {
        let api = Unit::new("PYTHIA", "API", UnitType::PackageBody);
        let pricing = Unit::new("PYTHIA", "PRICING", UnitType::PackageBody);
        let mut s = DebugSession::default();
        let mut actions = Vec::new();
        let stop = |variables| DebugEvent::Suspended {
            reason: StopReason::Breakpoint,
            location: Location {
                unit: pricing.clone(),
                line: Some(15),
            },
            stack: vec![
                frame(&pricing, "LINE_AMOUNT", Some(15)),
                frame(&api, "PROCESS", Some(41)),
            ],
            frame: 0,
            variables,
            watches: Vec::new(),
            exception: None,
        };
        s.on_event(stop(vec![var("P_QTY", Some("2"), vec![])]), &mut actions);
        assert_eq!(s.hover(&pricing, "p_qty").unwrap().1, "2");
        assert_eq!(s.hover(&api, "l_total"), None);

        // The caller: its source is shown, its values come from the debugger.
        let shown = s.select_frame(1);
        assert_eq!(
            shown,
            Some(DebugAction::ShowLocation(Location {
                unit: api.clone(),
                line: Some(41)
            }))
        );
        assert_eq!(s.frame, 1);
        assert!(s.variables.is_empty(), "not the other frame's values");
        assert_eq!(s.hover(&pricing, "p_qty"), None);
        s.on_event(
            DebugEvent::FrameSelected {
                frame: 1,
                variables: vec![var("L_TOTAL", Some("0"), vec![])],
                watches: Vec::new(),
            },
            &mut actions,
        );
        assert_eq!(s.hover(&api, "l_total").unwrap().1, "0");
        assert_eq!(s.hover(&pricing, "l_total"), None);
        // An answer for an earlier selection doesn't replace it.
        s.on_event(
            DebugEvent::FrameSelected {
                frame: 0,
                variables: Vec::new(),
                watches: Vec::new(),
            },
            &mut actions,
        );
        assert_eq!(s.variables.len(), 1);
        // The next stop starts at the innermost frame again.
        s.on_event(stop(Vec::new()), &mut actions);
        assert_eq!(s.frame, 0);
        assert_eq!(
            s.current_frame().map(|f| f.method.as_str()),
            Some("LINE_AMOUNT")
        );
    }

    #[test]
    fn paths_as_watches_name_them() {
        assert_eq!(child_path("", "L_REC"), "l_rec");
        assert_eq!(child_path("l_rec", "NAME"), "l_rec.name");
        assert_eq!(child_path("l_tab", "(3)"), "l_tab(3)");
        // Text keys keep their case, or the path finds nothing.
        assert_eq!(child_path("l_map", "('Key')"), "l_map('Key')");
        assert_eq!(child_path("l_map('Key')", "A"), "l_map('Key').a");
        assert_eq!(child_path("", "l_map('Key')"), "l_map('Key')", "a watch");
        let path = condition::parse_path(&child_path("l_map", "('Key')")).unwrap();
        let map = var("L_MAP", None, vec![var("('Key')", Some("1"), vec![])]);
        assert_eq!(
            condition::lookup(&[map], &path).unwrap().value.as_deref(),
            Some("1")
        );
    }

    #[test]
    fn breakpoints_and_watches_are_saved() {
        let mut s = DebugSession::default();
        let unit = Unit::new("PYTHIA", "P", UnitType::Procedure);
        s.toggle_breakpoint(unit.clone(), 5);
        s.toggle_breakpoint(unit.clone(), 9);
        let at = |line| Breakpoint {
            unit: unit.clone(),
            line,
        };
        s.set_enabled(&at(9), false);
        let options = BreakpointOptions {
            condition: Some("l_total > 3".into()),
            hit_count: Some(2),
        };
        s.set_options(&at(5), options.clone());
        s.add_watch("l_rec.name");
        s.set_break_on_exception(true);

        let saved = s.saved();
        let mut restored = DebugSession::default();
        restored.restore(saved.clone());
        assert_eq!(restored.breakpoints, s.breakpoints);
        assert_eq!(restored.disabled, s.disabled);
        assert_eq!(restored.options.get(&at(5)), Some(&options));
        assert_eq!(restored.watches, ["l_rec.name"]);
        assert!(restored.break_on_exception);
        assert_eq!(restored.markers(&unit).0, vec![5], "line 9 is disabled");
        assert_eq!(restored.saved(), saved);
    }

    #[test]
    fn values_change_while_stopped() {
        let mut s = DebugSession::default();
        s.set_value("l_total", Some("5".into()));
        assert!(
            s.value_error.is_none(),
            "nothing to change while not stopped"
        );
        s.state = State::Suspended {
            reason: StopReason::Breakpoint,
        };
        let mut actions = Vec::new();
        s.on_event(
            DebugEvent::ValueSet {
                expression: "l_np".into(),
                error: Some("1234.5 is too large for NUMBER(5,2)".into()),
            },
            &mut actions,
        );
        assert_eq!(
            s.value_error,
            Some(("l_np".into(), "1234.5 is too large for NUMBER(5,2)".into()))
        );
        assert_eq!(
            s.log.last().map(String::as_str),
            Some("Can't set l_np: 1234.5 is too large for NUMBER(5,2)")
        );
        // The next stop forgets it.
        let unit = Unit::new("PYTHIA", "P", UnitType::Procedure);
        s.on_event(
            DebugEvent::Suspended {
                reason: StopReason::Cursor,
                location: Location {
                    unit: unit.clone(),
                    line: Some(4),
                },
                stack: vec![frame(&unit, "P", Some(4))],
                frame: 0,
                variables: Vec::new(),
                watches: Vec::new(),
                exception: None,
            },
            &mut actions,
        );
        assert!(s.value_error.is_none());
    }

    /// Renders the panel while a value is changed and another one was
    /// refused, to `PYTHIA_SCREENSHOT_DIR`.
    #[test]
    fn changing_values_screenshot() {
        let Some(dir) = std::env::var_os("PYTHIA_SCREENSHOT_DIR") else {
            return;
        };
        let unit = Unit::new("PYTHIA", "DBGV_PKG", UnitType::PackageBody);
        let typed = |name: &str, type_name: &str, value: Option<&str>| Variable {
            type_name: type_name.into(),
            ..var(name, value, vec![])
        };
        for dark in [true, false] {
            let mut s = DebugSession {
                state: State::Suspended {
                    reason: StopReason::Cursor,
                },
                stack: vec![frame(&unit, "RUN", Some(23))],
                variables: vec![
                    typed("P_N", "NUMBER", Some("3")),
                    typed("L_TOTAL", "NUMBER", Some("3")),
                    typed("L_NAME", "VARCHAR2", None),
                    typed("L_NP", "NUMBER", Some("1.25")),
                    typed("L_DATE", "DATE", Some("29-FEB-24 12.00.00 AM")),
                ],
                watches: vec!["dbgv_pkg.g_num".into()],
                watch_values: vec![Watch {
                    expression: "dbgv_pkg.g_num".into(),
                    value: Ok(typed("G_NUM", "NUMBER", Some("8"))),
                }],
                value_edit: Some(ValueEdit {
                    path: "l_total".into(),
                    draft: "100".into(),
                    focused: false,
                }),
                value_error: Some(("l_np".into(), "1234.5 is too large for NUMBER(5,2)".into())),
                ..Default::default()
            };
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(1100.0, 260.0))
                .with_pixels_per_point(1.5)
                .wgpu()
                .build_ui(move |ui| {
                    egui::Frame::central_panel(ui.style()).show(ui, |ui| {
                        s.ui(ui);
                    });
                });
            harness.ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            harness.run_steps(4);
            let image = harness.render().expect("render");
            let name = format!(
                "debugger-set-value-{}.png",
                if dark { "dark" } else { "light" }
            );
            let path = std::path::Path::new(&dir).join(name);
            image.save(&path).unwrap();
            eprintln!("wrote {}", path.display());
        }
    }

    #[test]
    fn stepping_over_code_without_debug_information_is_logged() {
        let mut s = DebugSession::default();
        s.on_event(
            DebugEvent::SteppedOver(vec![Unit::new("PYTHIA", "PRICING", UnitType::PackageBody)]),
            &mut Vec::new(),
        );
        assert_eq!(
            s.log,
            [
                "Step into went over package body PYTHIA.PRICING: it was compiled without debug \
              information (Settings → Debugger)"
            ]
        );
    }
}
