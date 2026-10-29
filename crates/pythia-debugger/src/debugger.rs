//! The debugger side: listens for the target session, then drives it over
//! JDWP on a worker thread. The UI talks to it through channels and never
//! blocks.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::condition;
use crate::jdwp::{self, Event, Jdwp, JdwpError, Value};
use crate::types::{
    Breakpoint, BreakpointOptions, DebugCommand, DebugEvent, Frame, Location, StopReason, Unit,
    UnitType, Variable, Watch,
};
use crate::value::{self, Constraints};

/// Handle to a debugger worker.
pub struct Debugger {
    commands: Sender<DebugCommand>,
    events: Receiver<DebugEvent>,
    port: u16,
}

impl Debugger {
    /// Binds a listener (e.g. `0.0.0.0:0` for any free port) and starts the
    /// worker. `notify` is called whenever an event is available.
    pub fn listen(bind: &str, notify: impl Fn() + Send + 'static) -> io::Result<Self> {
        Self::with_listener(TcpListener::bind(bind)?, notify)
    }

    /// Starts the worker on a bound listener (see [`crate::setup::listen`]).
    pub fn with_listener(
        listener: TcpListener,
        notify: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        let events = Events {
            tx: ev_tx,
            notify: Box::new(notify),
        };
        thread::Builder::new()
            .name(format!("debugger-{port}"))
            .spawn(move || run(listener, cmd_rx, events))
            .expect("spawning debugger thread");
        Ok(Self {
            commands: cmd_tx,
            events: ev_rx,
            port,
        })
    }

    /// Port the target session must connect to.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn send(&self, command: DebugCommand) {
        let _ = self.commands.send(command);
    }

    pub fn try_recv(&self) -> Option<DebugEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for Debugger {
    fn drop(&mut self) {
        let _ = self.commands.send(DebugCommand::Detach);
    }
}

struct Events {
    tx: Sender<DebugEvent>,
    notify: Box<dyn Fn() + Send>,
}

impl Events {
    fn send(&self, event: DebugEvent) {
        if self.tx.send(event).is_ok() {
            (self.notify)();
        }
    }
}

fn run(listener: TcpListener, commands: Receiver<DebugCommand>, events: Events) {
    let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
    events.send(DebugEvent::Listening { port });
    let mut early: Vec<DebugCommand> = Vec::new();
    // Wait for the target, collecting breakpoints set meanwhile.
    let stream = loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                info!(%addr, "debug target connected");
                break stream;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => {
                events.send(DebugEvent::Error(format!("accepting debug target: {e}")));
                return;
            }
        }
        match commands.recv_timeout(Duration::from_millis(50)) {
            Ok(DebugCommand::Detach | DebugCommand::Terminate)
            | Err(mpsc::RecvTimeoutError::Disconnected) => {
                events.send(DebugEvent::Terminated);
                return;
            }
            Ok(cmd) => early.push(cmd),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    let result = stream
        .set_nonblocking(false)
        .map_err(JdwpError::from)
        .and_then(|()| Session::start(stream, &events));
    let mut session = match result {
        Ok(s) => s,
        Err(e) => {
            events.send(DebugEvent::Error(format!("debug handshake failed: {e}")));
            events.send(DebugEvent::Terminated);
            return;
        }
    };
    for cmd in early {
        session.command(cmd, &events);
    }
    if let Err(e) = session.event_loop(&commands, &events)
        && !matches!(e, JdwpError::Disconnected)
    {
        events.send(DebugEvent::Error(e.to_string()));
    }
    events.send(DebugEvent::Terminated);
}

/// JDWP state of one debug session.
struct Session {
    jdwp: Jdwp,
    /// Requested breakpoints.
    breakpoints: BTreeSet<Breakpoint>,
    /// Bound breakpoints → JDWP request ids.
    bound: HashMap<Breakpoint, i32>,
    /// Loaded classes by unit.
    classes: HashMap<Unit, u64>,
    /// Thread that is currently suspended at a breakpoint/step.
    stopped: Option<u64>,
    /// Classes whose line tables were cached: (class, method) → lines.
    lines: HashMap<(u64, u64), Vec<(u64, i32)>>,
    method_names: HashMap<(u64, u64), String>,
    /// Conditions and hit counts, and hits so far (condition true).
    options: HashMap<Breakpoint, BreakpointOptions>,
    hits: HashMap<Breakpoint, u32>,
    watches: Vec<String>,
    /// The JDWP request for exception events, when breaking on exceptions.
    exception_request: Option<i32>,
    /// The subprogram to stop in once ([`DebugCommand::BreakOnEntry`]),
    /// and its JDWP breakpoints (one per overload) once its unit is loaded.
    entry: Option<(Unit, String)>,
    entry_requests: Vec<i32>,
    /// The line to stop at once ([`DebugCommand::RunTo`]), and its JDWP
    /// breakpoint once its unit is loaded. (In an anonymous block, where
    /// the agent sets no breakpoints, a step goes there instead.)
    run_to: Option<Breakpoint>,
    run_to_request: Option<i32>,
    /// Why the thread stopped last, and the exception, to report the stop
    /// again when a run to a line can't start.
    last_stop: Option<(StopReason, Option<String>)>,
    /// The step in progress, if any.
    step: Option<Step>,
    /// Units without debug information loaded during the step into in
    /// progress (see [`DebugEvent::SteppedOver`]).
    stepped_over: Vec<Unit>,
    /// The stopped thread's frames, innermost first.
    frames: Vec<(u64, jdwp::Location)>,
    /// The frame watches are evaluated in while suspended.
    selected: Option<SelectedFrame>,
}

/// A step request and where it started. Oracle reports every code index
/// as a step, often several on one line; the step goes on until the line
/// or the frame changes.
struct Step {
    request: i32,
    thread: u64,
    /// `jdwp::step::INTO`, `OVER` or `OUT`.
    depth: i32,
    /// Stack depth, method and line where the step started.
    frames: usize,
    class: u64,
    method: u64,
    line: Option<u32>,
    /// Steps taken without leaving the line.
    repeats: u32,
    /// Stepping on to this line of a block (class, line): see
    /// [`Session::run_to_in_block`].
    until: Option<(u64, u32)>,
}

/// Steps on one line before stopping anyway (a one-line loop).
const MAX_SAME_LINE_STEPS: u32 = 1000;

/// The selected frame of the stopped thread and its variables.
struct SelectedFrame {
    /// Index in [`Session::frames`].
    index: usize,
    class: u64,
    variables: Vec<Variable>,
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

impl Session {
    fn start(stream: TcpStream, events: &Events) -> Result<Self, JdwpError> {
        let mut jdwp = Jdwp::handshake(stream)?;
        let version = jdwp.version().unwrap_or_else(|e| format!("unknown ({e})"));
        events.send(DebugEvent::Connected { version });
        let mut session = Session {
            jdwp,
            breakpoints: BTreeSet::new(),
            bound: HashMap::new(),
            classes: HashMap::new(),
            stopped: None,
            lines: HashMap::new(),
            method_names: HashMap::new(),
            options: HashMap::new(),
            hits: HashMap::new(),
            watches: Vec::new(),
            exception_request: None,
            entry: None,
            entry_requests: Vec::new(),
            run_to: None,
            run_to_request: None,
            last_stop: None,
            step: None,
            stepped_over: Vec::new(),
            frames: Vec::new(),
            selected: None,
        };
        for (_, id, signature) in session.jdwp.all_classes()? {
            if let Some(unit) = Unit::from_class_signature(&signature) {
                session.classes.insert(unit, id);
            }
        }
        // Every PL/SQL unit loaded later is announced, so deferred
        // breakpoints can be bound.
        session
            .jdwp
            .set_event(jdwp::kind::CLASS_PREPARE, jdwp::suspend::ALL, vec![])?;
        Ok(session)
    }

    /// Handles a UI command. Returns whether the target should be resumed.
    fn command(&mut self, command: DebugCommand, events: &Events) -> bool {
        let result: Result<bool, JdwpError> = (|| {
            Ok(match command {
                DebugCommand::SetBreakpoint(bp) => {
                    self.breakpoints.insert(bp.clone());
                    if let Some(class) = self.classes.get(&bp.unit).copied() {
                        self.bind(&bp, class, events)?;
                    }
                    false
                }
                DebugCommand::ClearBreakpoint(bp) => {
                    self.breakpoints.remove(&bp);
                    if let Some(request) = self.bound.remove(&bp) {
                        self.jdwp.clear_event(jdwp::kind::BREAKPOINT, request)?;
                    }
                    false
                }
                DebugCommand::SetBreakpointOptions(bp, options) => {
                    if options.is_empty() {
                        self.options.remove(&bp);
                    } else {
                        self.options.insert(bp, options);
                    }
                    false
                }
                DebugCommand::SetWatches(watches) => {
                    self.watches = watches;
                    if self.selected.is_some() {
                        let values = self.evaluate_watches();
                        events.send(DebugEvent::Watches(values));
                    }
                    false
                }
                DebugCommand::SelectFrame(frame) => {
                    if let Some(thread) = self.stopped
                        && frame < self.frames.len()
                    {
                        let variables = self.select_frame(thread, frame)?;
                        let watches = self.evaluate_watches();
                        events.send(DebugEvent::FrameSelected {
                            frame,
                            variables,
                            watches,
                        });
                    }
                    false
                }
                DebugCommand::BreakOnException(on) => {
                    match (on, self.exception_request) {
                        (true, None) => {
                            let modifier = self.jdwp.modifier_exception(true, true);
                            self.exception_request = Some(self.jdwp.set_event(
                                jdwp::kind::EXCEPTION,
                                jdwp::suspend::ALL,
                                vec![modifier],
                            )?);
                        }
                        (false, Some(request)) => {
                            self.exception_request = None;
                            self.jdwp.clear_event(jdwp::kind::EXCEPTION, request)?;
                        }
                        _ => {}
                    }
                    false
                }
                DebugCommand::BreakOnEntry { unit, subprogram } => {
                    self.clear_entry()?;
                    self.entry = Some((unit.clone(), subprogram));
                    if let Some(class) = self.classes.get(&unit).copied() {
                        self.bind_entry(class, events)?;
                    }
                    false
                }
                DebugCommand::SetValue { expression, value } => {
                    if let Some(thread) = self.stopped {
                        let error = self.set_value(&expression, value.as_deref()).err();
                        let changed = error.is_none();
                        events.send(DebugEvent::ValueSet { expression, error });
                        if changed && let Some(frame) = self.selected.as_ref().map(|s| s.index) {
                            let variables = self.select_frame(thread, frame)?;
                            let watches = self.evaluate_watches();
                            events.send(DebugEvent::FrameSelected {
                                frame,
                                variables,
                                watches,
                            });
                        }
                    }
                    false
                }
                DebugCommand::Resume => self.stopped.is_some(),
                DebugCommand::RunTo(target) => {
                    let Some(thread) = self.stopped else {
                        return Ok(false);
                    };
                    let started = self.start_run_to(thread, target, events);
                    if !matches!(started, Ok(true)) {
                        if let Err(e) = started {
                            events.send(DebugEvent::Error(e.to_string()));
                        }
                        // Still stopped where it was (the UI expects it to run).
                        let (reason, exception) =
                            self.last_stop.clone().unwrap_or((StopReason::Pause, None));
                        self.suspended(thread, reason, exception, events)?;
                        return Ok(false);
                    }
                    true
                }
                DebugCommand::StepOver | DebugCommand::StepInto | DebugCommand::StepOut => {
                    let Some(thread) = self.stopped else {
                        return Ok(false);
                    };
                    let depth = match command {
                        DebugCommand::StepOver => jdwp::step::OVER,
                        DebugCommand::StepInto => jdwp::step::INTO,
                        _ => jdwp::step::OUT,
                    };
                    self.stepped_over.clear();
                    self.start_step(thread, depth, 0)?;
                    true
                }
                DebugCommand::Pause => {
                    if self.stopped.is_none() {
                        self.jdwp.suspend_vm()?;
                        self.cancel_step();
                        if let Some(thread) = self.jdwp.all_threads()?.first().copied() {
                            self.suspended(thread, StopReason::Pause, None, events)?;
                        }
                    }
                    false
                }
                DebugCommand::Detach | DebugCommand::Terminate => false,
            })
        })();
        match result {
            Ok(resume) => resume,
            Err(e) => {
                events.send(DebugEvent::Error(e.to_string()));
                false
            }
        }
    }

    fn resume(&mut self, events: &Events) -> Result<(), JdwpError> {
        self.stopped = None;
        self.frames.clear();
        self.selected = None;
        self.jdwp.resume_vm()?;
        events.send(DebugEvent::Resumed);
        Ok(())
    }

    fn event_loop(
        &mut self,
        commands: &Receiver<DebugCommand>,
        events: &Events,
    ) -> Result<(), JdwpError> {
        // The target is suspended after VM_START; the queued VM_START event
        // batch below resumes it like any other non-stopping event.
        loop {
            loop {
                match commands.try_recv() {
                    Ok(DebugCommand::Detach) => {
                        let _ = self.jdwp.dispose();
                        return Ok(());
                    }
                    Ok(DebugCommand::Terminate) => {
                        let _ = self.jdwp.exit(1);
                        return Ok(());
                    }
                    Ok(cmd) => {
                        if self.command(cmd, events) {
                            self.resume(events)?;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        let _ = self.jdwp.dispose();
                        return Ok(());
                    }
                }
            }
            let Some((policy, batch)) = self.jdwp.next_events(Duration::from_millis(50))? else {
                continue;
            };
            let mut flow = Flow::Continue;
            // A breakpoint, an exception and a step can report the same
            // stop; handle it once. Every breakpoint counts its hit; it stops
            // when its condition and hit count say so.
            let mut breakpoint: Option<u64> = None;
            let mut exception: Option<(u64, String)> = None;
            let mut stepped: Option<u64> = None;
            let mut entered: Option<u64> = None;
            let mut reached: Option<u64> = None;
            for event in batch {
                debug!(?event, "debug event");
                match event {
                    Event::VmDeath => return Ok(()),
                    Event::ClassPrepare {
                        class, signature, ..
                    } => {
                        if let Some(unit) = Unit::from_class_signature(&signature) {
                            self.classes.insert(unit.clone(), class);
                            if self
                                .step
                                .as_ref()
                                .is_some_and(|s| s.depth == jdwp::step::INTO)
                                && user_code(&unit)
                                && !self.has_lines(class)?
                            {
                                self.stepped_over.push(unit.clone());
                            }
                            let pending: Vec<Breakpoint> = self
                                .breakpoints
                                .iter()
                                .filter(|b| b.unit == unit)
                                .cloned()
                                .collect();
                            for bp in pending {
                                self.bind(&bp, class, events)?;
                            }
                            if self.entry.as_ref().is_some_and(|(u, _)| *u == unit) {
                                self.bind_entry(class, events)?;
                            }
                            if self.run_to.as_ref().is_some_and(|b| b.unit == unit) {
                                self.bind_run_to(class, events)?;
                            }
                        }
                    }
                    Event::SingleStep { thread, .. } => stepped = Some(thread),
                    Event::Breakpoint {
                        request, thread, ..
                    } if self.entry_requests.contains(&request) => entered = Some(thread),
                    Event::Breakpoint {
                        request, thread, ..
                    } if self.run_to_request == Some(request) => reached = Some(thread),
                    Event::Breakpoint {
                        request,
                        thread,
                        location,
                    } => {
                        // Stepping from a breakpoint reports it again where
                        // the step starts; that isn't another hit.
                        if !self.at_step_origin(thread, &location)?
                            && self.breakpoint_stops(request, thread, events)?
                        {
                            breakpoint = Some(thread);
                        }
                    }
                    Event::Exception {
                        thread,
                        exception: e,
                        ..
                    } if exception.is_none() => {
                        exception = Some((thread, self.exception_text(e)));
                    }
                    _ => {}
                }
            }
            let stop = if let Some(thread) = breakpoint {
                Some((thread, StopReason::Breakpoint, None))
            } else if let Some((thread, text)) = exception {
                Some((thread, StopReason::Exception, Some(text)))
            } else if let Some(thread) = entered {
                Some((thread, StopReason::Entry, None))
            } else if let Some(thread) = reached {
                Some((thread, StopReason::Cursor, None))
            } else if let Some(thread) = stepped {
                let reason = if self.step.as_ref().is_some_and(|s| s.until.is_some()) {
                    StopReason::Cursor
                } else {
                    StopReason::Step
                };
                (!self.step_again(thread)?).then_some((thread, reason, None))
            } else {
                None
            };
            if let Some((thread, reason, exception)) = stop {
                // A step that ended elsewhere (at a breakpoint) must not fire
                // later, after the user continues.
                self.cancel_step();
                // The entry stops once, also when a breakpoint there
                // reported the stop.
                if entered.is_some() {
                    self.clear_entry()?;
                }
                self.suspended(thread, reason, exception, events)?;
                flow = Flow::Stop;
            }
            if flow == Flow::Continue && policy != jdwp::suspend::NONE {
                self.jdwp.resume_vm()?;
            }
        }
    }

    /// Requests a step of `depth` from the thread's current location.
    fn start_step(&mut self, thread: u64, depth: i32, repeats: u32) -> Result<(), JdwpError> {
        self.cancel_step();
        let frames = self.jdwp.frames(thread)?;
        let (class, method, line) = match frames.first() {
            Some((_, loc)) => (loc.class, loc.method, self.line_of(loc)?),
            None => (0, 0, None),
        };
        let modifier = self.jdwp.modifier_step(thread, depth);
        let request = self.jdwp.set_event(
            jdwp::kind::SINGLE_STEP,
            jdwp::suspend::ALL,
            vec![modifier, Jdwp::modifier_count(1)],
        )?;
        self.step = Some(Step {
            request,
            thread,
            depth,
            frames: frames.len(),
            class,
            method,
            line,
            repeats,
            until: None,
        });
        Ok(())
    }

    /// Clears the pending step request, if any.
    fn cancel_step(&mut self) {
        if let Some(step) = self.step.take() {
            // Fired steps are already gone; that error doesn't matter.
            let _ = self.jdwp.clear_event(jdwp::kind::SINGLE_STEP, step.request);
        }
    }

    /// Whether `location` is where the pending step started, in the same
    /// frame.
    fn at_step_origin(
        &mut self,
        thread: u64,
        location: &jdwp::Location,
    ) -> Result<bool, JdwpError> {
        let Some(step) = self.step.as_ref() else {
            return Ok(false);
        };
        if step.thread != thread || step.class != location.class || step.method != location.method {
            return Ok(false);
        }
        let (frames, line) = (step.frames, step.line);
        Ok(self.line_of(location)? == line && self.jdwp.frames(thread)?.len() == frames)
    }

    /// After a step event: whether it landed on the line it started from
    /// in the same frame, in which case the step goes on. Stepping on to a
    /// line of a block goes on until it's there.
    fn step_again(&mut self, thread: u64) -> Result<bool, JdwpError> {
        let Some(step) = self.step.as_ref() else {
            return Ok(false);
        };
        if let Some(until) = step.until {
            return self.step_toward(thread, until);
        }
        if step.depth == jdwp::step::OUT
            || step.thread != thread
            || step.line.is_none()
            || step.repeats >= MAX_SAME_LINE_STEPS
        {
            return Ok(false);
        }
        let (depth, frames, class, method, line, repeats) = (
            step.depth,
            step.frames,
            step.class,
            step.method,
            step.line,
            step.repeats,
        );
        let stack = self.jdwp.frames(thread)?;
        let Some((_, loc)) = stack.first() else {
            return Ok(false);
        };
        let same = stack.len() == frames
            && loc.class == class
            && loc.method == method
            && self.line_of(loc)? == line;
        if same {
            self.start_step(thread, depth, repeats + 1)?;
        }
        Ok(same)
    }

    /// Starts a run to a line ([`DebugCommand::RunTo`]) from a stop.
    /// Returns whether to resume: not when there's no code to stop at.
    fn start_run_to(
        &mut self,
        thread: u64,
        target: Breakpoint,
        events: &Events,
    ) -> Result<bool, JdwpError> {
        self.clear_run_to()?;
        let class = self.classes.get(&target.unit).copied();
        if target.unit.unit_type == UnitType::Block {
            let Some(class) = class else {
                events.send(DebugEvent::Error(format!(
                    "Can't run to line {}: that block isn't running",
                    target.line
                )));
                return Ok(false);
            };
            return self.run_to_in_block(thread, class, target, events);
        }
        self.run_to = Some(target);
        match class {
            Some(class) => self.bind_run_to(class, events),
            // Set when the unit is loaded.
            None => Ok(true),
        }
    }

    /// A run to a line of an anonymous block, where the agent sets no
    /// breakpoints (INVALID_LOCATION): steps out to the block, then over
    /// its lines until the line is reached. Breakpoints and exceptions on
    /// the way stop it as they would a breakpoint's run.
    fn run_to_in_block(
        &mut self,
        thread: u64,
        class: u64,
        target: Breakpoint,
        events: &Events,
    ) -> Result<bool, JdwpError> {
        let Some((line, _)) = self.code_at(class, target.line)? else {
            events.send(DebugEvent::Error(format!(
                "Can't run to line {} of the block: no code at or after it",
                target.line
            )));
            return Ok(false);
        };
        self.cancel_step();
        self.stepped_over.clear();
        self.step_toward(thread, (class, line))?;
        Ok(true)
    }

    /// Takes the next step toward a line of a block (class, line): over
    /// lines of the block, out of what it called. Returns false when the
    /// thread is there.
    fn step_toward(&mut self, thread: u64, until: (u64, u32)) -> Result<bool, JdwpError> {
        let (class, line) = until;
        let stack = self.jdwp.frames(thread)?;
        let Some((_, loc)) = stack.first() else {
            return Ok(false);
        };
        let in_block = loc.class == class;
        // The line it is on when the run starts doesn't count.
        let started = self.step.as_ref().is_some_and(|s| s.until.is_some());
        if started && in_block && self.line_of(loc)? == Some(line) {
            return Ok(false);
        }
        let depth = if in_block {
            jdwp::step::OVER
        } else {
            jdwp::step::OUT
        };
        self.start_step(thread, depth, 0)?;
        if let Some(step) = self.step.as_mut() {
            step.until = Some(until);
        }
        Ok(true)
    }

    /// Sets a JDWP breakpoint for `bp` in a loaded class.
    fn bind(&mut self, bp: &Breakpoint, class: u64, events: &Events) -> Result<(), JdwpError> {
        if let Some(old) = self.bound.remove(bp) {
            let _ = self.jdwp.clear_event(jdwp::kind::BREAKPOINT, old);
        }
        let Some((line, location)) = self.code_at(class, bp.line)? else {
            events.send(DebugEvent::BreakpointBound {
                breakpoint: bp.clone(),
                line: None,
                error: Some("no code at or after this line (compiled with debug info?)".into()),
            });
            return Ok(());
        };
        let modifier = self.jdwp.modifier_location(&location);
        let request =
            self.jdwp
                .set_event(jdwp::kind::BREAKPOINT, jdwp::suspend::ALL, vec![modifier])?;
        self.bound.insert(bp.clone(), request);
        events.send(DebugEvent::BreakpointBound {
            breakpoint: bp.clone(),
            line: Some(line),
            error: None,
        });
        Ok(())
    }

    /// The first line at or after `line` in a loaded class that has code,
    /// and its location.
    fn code_at(
        &mut self,
        class: u64,
        line: u32,
    ) -> Result<Option<(u32, jdwp::Location)>, JdwpError> {
        let mut best: Option<(i32, u64, u64)> = None;
        for method in self.jdwp.methods(class)? {
            for (index, l) in self.line_table(class, method.id)? {
                if l >= line as i32 && best.is_none_or(|(b, ..)| l < b) {
                    best = Some((l, method.id, index));
                }
            }
        }
        Ok(best.map(|(line, method, index)| {
            let location = jdwp::Location {
                type_tag: 1,
                class,
                method,
                index,
            };
            (line as u32, location)
        }))
    }

    /// Sets the JDWP breakpoint of the line to run to in its loaded class.
    /// Returns whether there is code to stop at; if not, it's dropped.
    fn bind_run_to(&mut self, class: u64, events: &Events) -> Result<bool, JdwpError> {
        let Some(target) = self.run_to.clone() else {
            return Ok(false);
        };
        let Some((_, location)) = self.code_at(class, target.line)? else {
            self.run_to = None;
            events.send(DebugEvent::Error(format!(
                "Can't run to line {} of {}: no code at or after it",
                target.line, target.unit
            )));
            return Ok(false);
        };
        let modifier = self.jdwp.modifier_location(&location);
        match self
            .jdwp
            .set_event(jdwp::kind::BREAKPOINT, jdwp::suspend::ALL, vec![modifier])
        {
            Ok(request) => {
                self.run_to_request = Some(request);
                Ok(true)
            }
            // Refused there: the run goes on (or stays stopped) without it.
            Err(JdwpError::Reply { code }) => {
                self.run_to = None;
                events.send(DebugEvent::Error(format!(
                    "Can't run to line {} of {}: {}",
                    target.line,
                    target.unit,
                    jdwp::error_name(code)
                )));
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Forgets the line to run to and clears its breakpoint.
    fn clear_run_to(&mut self) -> Result<(), JdwpError> {
        self.run_to = None;
        if let Some(request) = self.run_to_request.take() {
            self.jdwp.clear_event(jdwp::kind::BREAKPOINT, request)?;
        }
        Ok(())
    }

    /// Sets a JDWP breakpoint at the first line of every overload of the
    /// entry subprogram in its loaded class.
    fn bind_entry(&mut self, class: u64, events: &Events) -> Result<(), JdwpError> {
        let Some((unit, subprogram)) = self.entry.clone() else {
            return Ok(());
        };
        let mut found = false;
        for method in self.jdwp.methods(class)? {
            if !method.name.eq_ignore_ascii_case(&subprogram) {
                continue;
            }
            found = true;
            let first = self
                .line_table(class, method.id)?
                .into_iter()
                .min_by_key(|(index, _)| *index);
            let Some((index, _)) = first else {
                continue;
            };
            let location = jdwp::Location {
                type_tag: 1,
                class,
                method: method.id,
                index,
            };
            let modifier = self.jdwp.modifier_location(&location);
            let request =
                self.jdwp
                    .set_event(jdwp::kind::BREAKPOINT, jdwp::suspend::ALL, vec![modifier])?;
            self.entry_requests.push(request);
        }
        if self.entry_requests.is_empty() {
            let name = subprogram.to_lowercase();
            events.send(DebugEvent::Error(if found {
                format!("Can't stop in {name}: {unit} has no debug information")
            } else {
                format!("Can't stop in {name}: {unit} has no such subprogram")
            }));
        }
        Ok(())
    }

    /// Forgets the entry subprogram and clears its breakpoints.
    fn clear_entry(&mut self) -> Result<(), JdwpError> {
        self.entry = None;
        for request in std::mem::take(&mut self.entry_requests) {
            self.jdwp.clear_event(jdwp::kind::BREAKPOINT, request)?;
        }
        Ok(())
    }

    fn line_table(&mut self, class: u64, method: u64) -> Result<Vec<(u64, i32)>, JdwpError> {
        if let Some(lines) = self.lines.get(&(class, method)) {
            return Ok(lines.clone());
        }
        let lines = match self.jdwp.line_table(class, method) {
            Ok(t) => t.lines,
            Err(JdwpError::Reply { .. }) => Vec::new(),
            Err(e) => return Err(e),
        };
        self.lines.insert((class, method), lines.clone());
        Ok(lines)
    }

    /// Whether any subprogram of a class has line information (it was
    /// compiled with debug information).
    fn has_lines(&mut self, class: u64) -> Result<bool, JdwpError> {
        let methods = match self.jdwp.methods(class) {
            Ok(m) => m,
            // Unknown: don't claim it has none.
            Err(JdwpError::Reply { .. }) => return Ok(true),
            Err(e) => return Err(e),
        };
        for method in methods {
            if !self.line_table(class, method.id)?.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn line_of(&mut self, loc: &jdwp::Location) -> Result<Option<u32>, JdwpError> {
        let lines = self.line_table(loc.class, loc.method)?;
        Ok(lines
            .iter()
            .filter(|(i, _)| *i <= loc.index)
            .max_by_key(|(i, _)| *i)
            .map(|(_, l)| *l as u32))
    }

    fn method_name(&mut self, class: u64, method: u64) -> Result<String, JdwpError> {
        if let Some(n) = self.method_names.get(&(class, method)) {
            return Ok(n.clone());
        }
        for m in self.jdwp.methods(class)? {
            self.method_names.insert((class, m.id), m.name);
        }
        Ok(self
            .method_names
            .get(&(class, method))
            .cloned()
            .unwrap_or_default())
    }

    fn unit_of(&mut self, class: u64) -> Result<Unit, JdwpError> {
        if let Some((unit, _)) = self.classes.iter().find(|(_, c)| **c == class) {
            return Ok(unit.clone());
        }
        let signature = self.jdwp.signature(class)?;
        let unit = Unit::from_class_signature(&signature).unwrap_or_else(|| {
            Unit::new(
                "",
                signature.trim_start_matches('L').trim_end_matches(';'),
                crate::types::UnitType::Block,
            )
        });
        self.classes.insert(unit.clone(), class);
        Ok(unit)
    }

    /// Reports a stop with the stack and the variables of the innermost
    /// frame with source, which is selected.
    fn suspended(
        &mut self,
        thread: u64,
        reason: StopReason,
        exception: Option<String>,
        events: &Events,
    ) -> Result<(), JdwpError> {
        // Any stop ends a run to a line.
        self.clear_run_to()?;
        self.last_stop = Some((reason, exception.clone()));
        self.stopped = Some(thread);
        let frames = self.jdwp.frames(thread)?;
        let mut stack = Vec::new();
        for (_, loc) in &frames {
            let unit = self.unit_of(loc.class)?;
            let line = self.line_of(loc)?;
            let method = self.method_name(loc.class, loc.method)?;
            stack.push(Frame {
                location: Location { unit, line },
                method,
            });
        }
        self.frames = frames;
        let frame = stack
            .iter()
            .position(|f| f.location.line.is_some())
            .unwrap_or(0);
        let variables = self.select_frame(thread, frame)?;
        let watches = self.evaluate_watches();
        let location = stack
            .first()
            .map(|f| f.location.clone())
            .unwrap_or(Location {
                unit: Unit::new("", "?", crate::types::UnitType::Block),
                line: None,
            });
        let stepped_over = std::mem::take(&mut self.stepped_over);
        if reason == StopReason::Step && !stepped_over.is_empty() {
            events.send(DebugEvent::SteppedOver(stepped_over));
        }
        events.send(DebugEvent::Suspended {
            reason,
            location,
            stack,
            frame,
            variables,
            watches,
            exception,
        });
        Ok(())
    }

    /// Reads the variables of a frame of the stopped thread and makes it
    /// the one watches are evaluated in.
    fn select_frame(&mut self, thread: u64, index: usize) -> Result<Vec<Variable>, JdwpError> {
        let Some((frame, loc)) = self.frames.get(index).copied() else {
            self.selected = None;
            return Ok(Vec::new());
        };
        let variables = self.frame_variables(thread, frame, &loc)?;
        self.selected = Some(SelectedFrame {
            index,
            class: loc.class,
            variables: variables.clone(),
        });
        Ok(variables)
    }

    /// Whether a breakpoint event should stop: counts the hit when the
    /// condition holds and compares with the hit count. A condition that
    /// can't be evaluated stops (and says why).
    fn breakpoint_stops(
        &mut self,
        request: i32,
        thread: u64,
        events: &Events,
    ) -> Result<bool, JdwpError> {
        let Some(bp) = self
            .bound
            .iter()
            .find(|(_, r)| **r == request)
            .map(|(b, _)| b.clone())
        else {
            return Ok(true);
        };
        let options = self.options.get(&bp).cloned().unwrap_or_default();
        if let Some(condition) = &options.condition {
            let frames = self.jdwp.frames(thread)?;
            let Some((frame, loc)) = frames.first() else {
                return Ok(true);
            };
            let variables = self.frame_variables(thread, *frame, loc)?;
            let class = loc.class;
            let result = condition::evaluate(condition, &mut |path| {
                self.resolve(path, &variables, Some(class)).map(|v| v.value)
            });
            match result {
                Ok(true) => {}
                Ok(false) => return Ok(false),
                Err(e) => {
                    events.send(DebugEvent::Error(format!(
                        "Breakpoint condition at {} line {}: {e}",
                        bp.unit, bp.line
                    )));
                    return Ok(true);
                }
            }
        }
        let hits = self.hits.entry(bp.clone()).or_default();
        *hits += 1;
        let hits = *hits;
        events.send(DebugEvent::BreakpointHit {
            breakpoint: bp,
            hits,
        });
        Ok(hits >= options.hit_count.unwrap_or(1))
    }

    /// The watches' values in the selected frame.
    fn evaluate_watches(&mut self) -> Vec<Watch> {
        let Some(selected) = self.selected.take() else {
            return Vec::new();
        };
        let watches = self.watches.clone();
        let out = watches
            .into_iter()
            .map(|expression| {
                let value = self.resolve(&expression, &selected.variables, Some(selected.class));
                Watch { expression, value }
            })
            .collect();
        self.selected = Some(selected);
        out
    }

    /// Resolves a watch path: one of `variables` (of a frame), a global of
    /// the package of `class` (the frame's), or `package.variable`,
    /// followed by fields and elements.
    fn resolve(
        &mut self,
        expression: &str,
        variables: &[Variable],
        class: Option<u64>,
    ) -> Result<Variable, String> {
        let path = condition::parse_path_with(expression, &condition::subscripts(variables))
            .ok_or_else(|| format!("not a variable: {}", expression.trim()))?;
        let (first, rest) = path.split_first().expect("non-empty path");
        if let Some(var) = variables
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(first))
        {
            return condition::descend(var, rest).cloned();
        }
        // Globals of the unit we're in (package body and spec).
        let current = class.and_then(|c| {
            self.classes
                .iter()
                .find(|(_, id)| **id == c)
                .map(|(u, _)| u.clone())
        });
        if let Some(unit) = &current {
            for class in self.package_classes(&unit.owner, &unit.name) {
                if let Some(var) = self.static_variable(class, first)? {
                    return condition::descend(&var, rest).cloned();
                }
            }
        }
        // `package.variable`, preferring the current unit's schema.
        if let Some((name, rest)) = rest.split_first() {
            let owner = current
                .as_ref()
                .map(|u| u.owner.clone())
                .unwrap_or_default();
            let mut classes = self.package_classes(&owner, first);
            if classes.is_empty() {
                classes = self.package_classes("", first);
            }
            if classes.is_empty() {
                return Err(format!(
                    "{} is unknown (or its package isn't loaded yet)",
                    expression.trim()
                ));
            }
            for class in classes {
                if let Some(var) = self.static_variable(class, name)? {
                    return condition::descend(&var, rest).cloned();
                }
            }
            return Err(format!(
                "{} has no {}",
                first.to_lowercase(),
                name.to_lowercase()
            ));
        }
        Err(format!("{} is unknown here", expression.trim()))
    }

    /// Loaded classes of a package (body first, then spec), or of the unit
    /// itself for other kinds. An empty `owner` matches any schema.
    fn package_classes(&self, owner: &str, name: &str) -> Vec<u64> {
        let mut found: Vec<(&Unit, u64)> = self
            .classes
            .iter()
            .filter(|(u, _)| {
                u.name.eq_ignore_ascii_case(name)
                    && (owner.is_empty() || u.owner == owner)
                    && u.unit_type != UnitType::Block
            })
            .map(|(u, c)| (u, *c))
            .collect();
        found.sort_by_key(|(u, _)| match u.unit_type {
            UnitType::PackageBody => 0,
            UnitType::Package => 1,
            _ => 2,
        });
        found.into_iter().map(|(_, c)| c).collect()
    }

    /// A static field (package global) of a class, by name. Fails for a
    /// package that isn't instantiated in the session yet: its globals have
    /// no values (they aren't NULL either).
    fn static_variable(&mut self, class: u64, name: &str) -> Result<Option<Variable>, String> {
        let Some((field, value)) = self.static_value(class, name)? else {
            return Ok(None);
        };
        self.variable(field.name, &field.signature, &value, 0)
            .map(Some)
            .map_err(|e| e.to_string())
    }

    /// The field and value of [`Self::static_variable`].
    fn static_value(
        &mut self,
        class: u64,
        name: &str,
    ) -> Result<Option<(jdwp::FieldInfo, Value)>, String> {
        let fields = match self.jdwp.fields(class) {
            Ok(f) => f,
            Err(JdwpError::Reply { .. }) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let Some(field) = fields
            .iter()
            .find(|f| f.modifiers & STATIC != 0 && f.name.eq_ignore_ascii_case(name))
            .cloned()
        else {
            return Ok(None);
        };
        let value = match self.jdwp.static_values(class, &[field.id]) {
            Ok(mut v) => v.pop(),
            Err(JdwpError::Reply { .. }) => None,
            Err(e) => return Err(e.to_string()),
        };
        let Some(value) = value else {
            return Ok(None);
        };
        if matches!(value, Value::Object { id: 0, .. })
            && !self
                .instantiated(class, &fields)
                .map_err(|e| e.to_string())?
        {
            let package = self.unit_of(class).map_err(|e| e.to_string())?;
            return Err(format!(
                "{} isn't instantiated in this session yet: no values until it's first used \
                 (constants may be compiled into the code that reads them)",
                package.name.to_lowercase()
            ));
        }
        Ok(Some((field, value)))
    }

    /// Whether the package state behind `class` exists in the session.
    /// Before it does, the agent reports every global as a null reference;
    /// after, each is an object, also when its value is NULL.
    fn instantiated(&mut self, class: u64, fields: &[jdwp::FieldInfo]) -> Result<bool, JdwpError> {
        let globals: Vec<u64> = fields
            .iter()
            .filter(|f| f.modifiers & STATIC != 0 && !f.name.starts_with('_'))
            .map(|f| f.id)
            .collect();
        match self.jdwp.static_values(class, &globals) {
            Ok(values) => Ok(values
                .iter()
                .any(|v| !matches!(v, Value::Object { id: 0, .. }))),
            // Unknown: don't claim it isn't.
            Err(JdwpError::Reply { .. }) => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Changes a scalar of the selected frame ([`DebugCommand::SetValue`]).
    /// The agent keeps a scalar as an object whose `_value` field holds
    /// its text; it parses what is written there.
    fn set_value(&mut self, expression: &str, value: Option<&str>) -> Result<(), String> {
        let path = condition::parse_path(expression)
            .ok_or_else(|| format!("not a variable: {}", expression.trim()))?;
        let holder = self.holder(&path)?;
        let (_, class) = self.jdwp.object_type(holder).map_err(|e| e.to_string())?;
        let type_name = type_name(&self.jdwp.signature(class).map_err(|e| e.to_string())?);
        let fields = self.jdwp.fields(class).map_err(|e| e.to_string())?;
        let Some(value_field) = fields.iter().find(|f| f.name == "_value").map(|f| f.id) else {
            return Err(format!(
                "{} is a {type_name}: change its fields or elements",
                expression.trim()
            ));
        };
        let ids: Vec<u64> = fields.iter().map(|f| f.id).collect();
        let values = self
            .jdwp
            .object_values(holder, &ids)
            .map_err(|e| e.to_string())?;
        let mut constraints = Constraints::default();
        let mut current = None;
        for (field, v) in fields.iter().zip(values) {
            let int = |v: &Value| match v {
                Value::Int(i) => *i as i32,
                _ => 0,
            };
            match field.name.as_str() {
                "_value" => {
                    if let Value::String(id) = v
                        && id != 0
                    {
                        current = Some(self.jdwp.string_value(id).map_err(|e| e.to_string())?);
                    }
                }
                "_canBeNull" => constraints.can_be_null = v != Value::Bool(false),
                "_precision" => constraints.precision = int(&v),
                "_scale" => constraints.scale = int(&v),
                "_minValue" => constraints.min = int(&v),
                "_maxValue" => constraints.max = int(&v),
                "_maxLength" | "_length" => constraints.length = int(&v),
                _ => {}
            }
        }
        let texts = value::prepare(&type_name, &constraints, value, current.as_deref())?;
        let mut refused = 0;
        for text in texts {
            match self.write_value(holder, value_field, text.as_deref()) {
                Ok(()) => return Ok(()),
                Err(JdwpError::Reply { code }) => refused = code,
                Err(e) => return Err(e.to_string()),
            }
        }
        // A DATE the agent can't parse is changed anyway; put it back. (Its
        // text has a two-digit year where the session's format has one, so
        // a date outside 1950-2049 comes back in another century.)
        let _ = self.write_value(holder, value_field, current.as_deref());
        Err(value::refusal(
            &type_name,
            &constraints,
            refused,
            current.as_deref(),
        ))
    }

    /// Writes a scalar's `_value` (`None` = NULL).
    fn write_value(
        &mut self,
        holder: u64,
        field: u64,
        text: Option<&str>,
    ) -> Result<(), JdwpError> {
        let value = match text {
            Some(text) => Value::String(self.jdwp.create_string(text)?),
            None => Value::String(0),
        };
        self.jdwp.set_object_values(holder, &[(field, value)])
    }

    /// The object behind a path in the selected frame, like a watch: one of
    /// its variables, a global of its package or `package.variable`, then
    /// fields and elements.
    fn holder(&mut self, path: &[String]) -> Result<u64, String> {
        let (first, rest) = path.split_first().expect("non-empty path");
        let (mut object, rest) = self.root_object(first, rest)?;
        let mut name = first.to_lowercase();
        for segment in rest {
            object = self.member_object(object, &name, segment)?;
            name = shown_segment(segment);
        }
        Ok(object)
    }

    /// The object a path starts with, and the rest of the path.
    fn root_object<'p>(
        &mut self,
        first: &str,
        rest: &'p [String],
    ) -> Result<(u64, &'p [String]), String> {
        let (Some(thread), Some(index)) = (self.stopped, self.selected.as_ref().map(|s| s.index))
        else {
            return Err("not stopped".into());
        };
        let (frame, loc) = self.frames[index];
        let object = |value: Value| match value {
            Value::Object { id, .. } if id != 0 => Ok(id),
            _ => Err(format!("{} has no value to change", first.to_lowercase())),
        };
        let vars = match self.jdwp.variable_table(loc.class, loc.method) {
            Ok((_, vars)) => vars,
            Err(JdwpError::Reply { .. }) => Vec::new(),
            Err(e) => return Err(e.to_string()),
        };
        if let Some(var) = vars.iter().find(|v| {
            v.name.eq_ignore_ascii_case(first)
                && v.code_index <= loc.index
                && loc.index < v.code_index + v.length.max(0) as u64
        }) {
            let tag = var.signature.as_bytes().first().copied().unwrap_or(b'L');
            let mut values = self
                .jdwp
                .frame_values(thread, frame, &[(var.slot, tag)])
                .map_err(|e| e.to_string())?;
            return Ok((object(values.pop().unwrap_or(Value::Void))?, rest));
        }
        // Globals of the frame's package (body and spec).
        let unit = self.unit_of(loc.class).map_err(|e| e.to_string())?;
        for class in self.package_classes(&unit.owner, &unit.name) {
            if let Some((_, value)) = self.static_value(class, first)? {
                return Ok((object(value)?, rest));
            }
        }
        // `package.variable`, preferring the frame's schema.
        if let Some((name, rest)) = rest.split_first() {
            let mut classes = self.package_classes(&unit.owner, first);
            if classes.is_empty() {
                classes = self.package_classes("", first);
            }
            for class in classes {
                if let Some((_, value)) = self.static_value(class, name)? {
                    return Ok((object(value)?, rest));
                }
            }
        }
        Err(format!("{} is unknown here", first.to_lowercase()))
    }

    /// A field (`segment` is a name) or element (`(n)`, `('key')`) of a
    /// record, object or collection.
    fn member_object(&mut self, object: u64, name: &str, segment: &str) -> Result<u64, String> {
        let missing = || format!("{name} has no {}", shown_segment(segment));
        let e = |e: JdwpError| e.to_string();
        let (_, class) = self.jdwp.object_type(object).map_err(e)?;
        let fields = self.jdwp.fields(class).map_err(e)?;
        let field = |name: &str| fields.iter().find(|f| f.name == name).map(|f| f.id);
        let value = if segment.starts_with('(') {
            let values = field("_values").ok_or_else(missing)?;
            let Value::Object { id: array, .. } = self.object_field(object, values)? else {
                return Err(missing());
            };
            let len = self.jdwp.array_length(array).map_err(e)?;
            let elements = self.jdwp.array_values(array, 0, len).map_err(e)?;
            // A dense collection has element n at position n - 1: that first.
            let guess = segment
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| n.checked_sub(1))
                .filter(|i| *i < elements.len());
            let mut found = None;
            for i in guess.into_iter().chain(0..elements.len()) {
                let Value::Object { id, .. } = elements[i] else {
                    continue;
                };
                if condition::matches(&self.element_name(id, i)?, segment) {
                    found = Some(id);
                    break;
                }
            }
            let element = found.ok_or_else(missing)?;
            // An element wraps its value (a scalar, record or object).
            let (_, element_class) = self.jdwp.object_type(element).map_err(e)?;
            let inner = self
                .jdwp
                .fields(element_class)
                .map_err(e)?
                .into_iter()
                .find(|f| f.name == "_value")
                .ok_or_else(missing)?;
            self.object_field(element, inner.id)?
        } else {
            let id = fields
                .iter()
                .find(|f| !f.name.starts_with('_') && f.name.eq_ignore_ascii_case(segment))
                .map(|f| f.id)
                .ok_or_else(missing)?;
            self.object_field(object, id)?
        };
        match value {
            Value::Object { id, .. } if id != 0 => Ok(id),
            _ => Err(format!("{} has no value to change", shown_segment(segment))),
        }
    }

    fn object_field(&mut self, object: u64, field: u64) -> Result<Value, String> {
        let mut values = self
            .jdwp
            .object_values(object, &[field])
            .map_err(|e| e.to_string())?;
        values.pop().ok_or_else(|| "no value".into())
    }

    /// A collection element's name as [`Self::variable`] gives it: by its
    /// key, or `(n)` by position.
    fn element_name(&mut self, element: u64, position: usize) -> Result<String, String> {
        let e = |e: JdwpError| e.to_string();
        let (_, class) = self.jdwp.object_type(element).map_err(e)?;
        let fields = self.jdwp.fields(class).map_err(e)?;
        let Some(key) = fields.iter().find(|f| f.name == "_key").cloned() else {
            return Ok(format!("({})", position + 1));
        };
        let value = self.object_field(element, key.id)?;
        let key_var = self
            .variable(String::new(), &key.signature, &value, 1)
            .map_err(e)?;
        Ok(match key_var.value {
            Some(text) => element_name(&text, key_is_text(&key.signature)),
            None => format!("({})", position + 1),
        })
    }

    /// A readable description of a raised exception object.
    fn exception_text(&mut self, exception: u64) -> String {
        let value = Value::Object {
            tag: b'L',
            id: exception,
        };
        match self.variable(String::new(), "", &value, 0) {
            Ok(var) => describe_exception(&var),
            Err(e) => format!("exception ({e})"),
        }
    }

    fn frame_variables(
        &mut self,
        thread: u64,
        frame: u64,
        loc: &jdwp::Location,
    ) -> Result<Vec<Variable>, JdwpError> {
        let vars = match self.jdwp.variable_table(loc.class, loc.method) {
            Ok((_, vars)) => vars,
            Err(JdwpError::Reply { .. }) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        // Only variables in scope at the current code index.
        let vars: Vec<_> = vars
            .into_iter()
            .filter(|v| {
                v.code_index <= loc.index && loc.index < v.code_index + v.length.max(0) as u64
            })
            .collect();
        if vars.is_empty() {
            return Ok(Vec::new());
        }
        let slots: Vec<(i32, u8)> = vars
            .iter()
            .map(|v| {
                (
                    v.slot,
                    v.signature.as_bytes().first().copied().unwrap_or(b'L'),
                )
            })
            .collect();
        let values = match self.jdwp.frame_values(thread, frame, &slots) {
            Ok(v) => v,
            Err(JdwpError::Reply { code }) => {
                warn!("reading frame values failed: {}", jdwp::error_name(code));
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for (var, value) in vars.iter().zip(values) {
            out.push(self.variable(var.name.clone(), &var.signature, &value, 0)?);
        }
        Ok(out)
    }

    /// Converts a JDWP value into a variable; PL/SQL scalars are objects of
    /// `$Oracle/Builtin/<TYPE>` with a string `_value` field.
    fn variable(
        &mut self,
        name: String,
        signature: &str,
        value: &Value,
        depth: usize,
    ) -> Result<Variable, JdwpError> {
        let mut var = Variable {
            name,
            type_name: type_name(signature),
            value: None,
            children: Vec::new(),
        };
        match value {
            Value::Object { id: 0, .. } | Value::String(0) => {}
            Value::String(id) => var.value = Some(self.jdwp.string_value(*id)?),
            Value::Int(i) => var.value = Some(i.to_string()),
            Value::Float(f) => var.value = Some(f.to_string()),
            Value::Bool(b) => var.value = Some(b.to_string()),
            Value::Char(c) => var.value = Some(c.to_string()),
            Value::Void => {}
            Value::Object { tag: b'[', id } if depth < MAX_DEPTH => {
                let len = self.jdwp.array_length(*id)?;
                let values = self.jdwp.array_values(*id, 0, len.min(MAX_ELEMENTS))?;
                for (i, v) in values.iter().enumerate() {
                    // Named by position unless the element has a key.
                    let element = self.variable(format!("({})", i + 1), "", v, depth + 1)?;
                    var.children.push(element);
                }
                var.value = Some(if len > MAX_ELEMENTS {
                    format!("{len} elements (first {MAX_ELEMENTS} shown)")
                } else {
                    format!("{len} element(s)")
                });
            }
            Value::Object { id, .. } if depth < MAX_DEPTH => {
                let (_, class) = self.jdwp.object_type(*id)?;
                if var.type_name.is_empty() {
                    var.type_name = type_name(&self.jdwp.signature(class)?);
                }
                let fields = self.jdwp.fields(class)?;
                if fields.is_empty() && var.type_name == "OPAQUE" {
                    // SQL collection types (and the like): the agent shows
                    // no contents, but the value isn't NULL.
                    var.value = Some(OPAQUE_VALUE.into());
                    return Ok(var);
                }
                let ids: Vec<u64> = fields.iter().map(|f| f.id).collect();
                let values: Vec<Option<Value>> = match self.jdwp.object_values(*id, &ids) {
                    _ if ids.is_empty() => Vec::new(),
                    Ok(values) => values.into_iter().map(Some).collect(),
                    // Some fields (e.g. statics) can't be read through the
                    // object; read one by one and skip those.
                    Err(JdwpError::Reply { .. }) => ids
                        .iter()
                        .map(|f| {
                            self.jdwp
                                .object_values(*id, &[*f])
                                .ok()
                                .and_then(|mut v| v.pop())
                        })
                        .collect(),
                    Err(e) => return Err(e),
                };
                for (field, v) in fields.iter().zip(values) {
                    let Some(v) = v else { continue };
                    if field.name == "_values" {
                        // Collections: `_values` is an array of elements.
                        let collection =
                            self.variable(String::new(), &field.signature, &v, depth)?;
                        var.children = collection.children;
                        var.value = collection.value;
                    } else if field.name == "_value" {
                        match v {
                            Value::String(0) | Value::Object { id: 0, .. } => {}
                            Value::String(sid) => var.value = Some(self.jdwp.string_value(sid)?),
                            other => {
                                // A collection element wraps its value, which
                                // may be a record or object with fields.
                                let inner = self.variable(
                                    String::new(),
                                    &field.signature,
                                    &other,
                                    depth + 1,
                                )?;
                                if var.type_name.ends_with("$element") {
                                    var.type_name = inner.type_name;
                                }
                                var.value = inner.value;
                                var.children = inner.children;
                            }
                        }
                    } else if field.name == "_key" {
                        // Index-by tables: elements are named by their key.
                        let key = self.variable(String::new(), &field.signature, &v, depth + 1)?;
                        if let Some(key) = key.value {
                            var.name = element_name(&key, key_is_text(&field.signature));
                        }
                    } else if !field.name.starts_with('_') {
                        var.children.push(self.variable(
                            field.name.clone(),
                            &field.signature,
                            &v,
                            depth + 1,
                        )?);
                    }
                }
            }
            Value::Object { .. } => var.value = Some("…".into()),
        }
        Ok(var)
    }
}

/// A path segment for messages: a name lowercase, a key as it is.
fn shown_segment(segment: &str) -> String {
    if segment.starts_with('(') {
        segment.to_owned()
    } else {
        segment.to_lowercase()
    }
}

/// `ACC_STATIC` in JDWP field modifiers.
const STATIC: i32 = 0x0008;

/// Stored units with code that can be compiled with debug information to
/// step into: not specs or anonymous blocks, types declared in a unit
/// (`DBGX_PRICING/DBGX_ORDERS/Rowtype`), nor SYS's built-in packages.
fn user_code(unit: &Unit) -> bool {
    matches!(
        unit.unit_type,
        UnitType::Procedure
            | UnitType::Function
            | UnitType::PackageBody
            | UnitType::TypeBody
            | UnitType::Trigger
    ) && !unit.name.contains('/')
        && unit.owner != "SYS"
}

/// Builds a one-line description from an exception object. Oracle names
/// the class of `ORA-n` exceptions `EXCEPTION_ORA_n`.
fn describe_exception(var: &Variable) -> String {
    if let Some(code) = var
        .type_name
        .strip_prefix("EXCEPTION_ORA_")
        .and_then(|c| c.parse::<u32>().ok())
    {
        return match predefined_exception(code) {
            Some(name) => format!("ORA-{code:05} ({name})"),
            None => format!("ORA-{code:05}"),
        };
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = &var.value {
        parts.push(v.clone());
    }
    for child in &var.children {
        if let Some(v) = &child.value
            && !v.is_empty()
        {
            parts.push(format!("{}={v}", child.name.to_lowercase()));
        }
    }
    if parts.is_empty() {
        var.type_name.clone()
    } else {
        format!("{} {}", var.type_name, parts.join(", "))
            .trim()
            .to_owned()
    }
}

/// Names of PL/SQL's predefined exceptions.
fn predefined_exception(code: u32) -> Option<&'static str> {
    Some(match code {
        1 => "dup_val_on_index",
        51 => "timeout_on_resource",
        1001 => "invalid_cursor",
        1012 => "not_logged_on",
        1017 => "login_denied",
        1403 => "no_data_found",
        1422 => "too_many_rows",
        1476 => "zero_divide",
        1722 => "invalid_number",
        6500 => "storage_error",
        6501 => "program_error",
        6502 => "value_error",
        6504 => "rowtype_mismatch",
        6511 => "cursor_already_open",
        6530 => "access_into_null",
        6531 => "collection_is_null",
        6532 => "subscript_outside_limit",
        6533 => "subscript_beyond_count",
        6592 => "case_not_found",
        30625 => "self_is_null",
        _ => return None,
    })
}

/// Shown for values the agent doesn't expose (SQL collection types).
const OPAQUE_VALUE: &str = "(contents not available)";

/// Display name of a collection element by its key: `(3)` or `('L1')`.
fn element_name(key: &str, text: bool) -> String {
    if text {
        format!("('{}')", key.replace('\'', "''"))
    } else {
        format!("({key})")
    }
}

/// Whether an index-by table key of this type is text (`INDEX BY VARCHAR2`).
fn key_is_text(signature: &str) -> bool {
    condition::is_text(&type_name(signature))
}

/// Nesting limit for records/collections (each PL/SQL level is ~2 objects).
const MAX_DEPTH: usize = 8;
/// Collection elements read per variable.
const MAX_ELEMENTS: i32 = 200;

/// `L$Oracle/Builtin/VARCHAR2;` → `VARCHAR2`; user types keep owner.name;
/// types declared inside a unit (`…/DBG_DEMO/DBG_DEMO/T_ARR`) keep their
/// own name.
fn type_name(signature: &str) -> String {
    let inner = signature.trim_start_matches('L').trim_end_matches(';');
    if let Some(builtin) = inner.strip_prefix("$Oracle/Builtin/") {
        return builtin.to_owned();
    }
    if let Some(unit) = Unit::from_class_signature(signature) {
        if unit.name.contains('/') {
            return unit.name.rsplit('/').next().unwrap_or_default().to_owned();
        }
        return format!("{}.{}", unit.owner, unit.name);
    }
    inner.rsplit('/').next().unwrap_or(inner).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(type_name("L$Oracle/Builtin/VARCHAR2;"), "VARCHAR2");
        assert_eq!(type_name("L$Oracle/Type/HR/T_REC;"), "HR.T_REC");
        assert_eq!(
            type_name("L$Oracle/Procedure/PYTHIA/DBG_DEMO/DBG_DEMO/T_ARR;"),
            "T_ARR"
        );
        assert_eq!(type_name(""), "");
    }

    #[test]
    fn exception_descriptions() {
        let exception = |type_name: &str| Variable {
            name: String::new(),
            type_name: type_name.into(),
            value: None,
            children: Vec::new(),
        };
        assert_eq!(
            describe_exception(&exception("EXCEPTION_ORA_1476")),
            "ORA-01476 (zero_divide)"
        );
        assert_eq!(
            describe_exception(&exception("EXCEPTION_ORA_20001")),
            "ORA-20001"
        );
        assert_eq!(describe_exception(&exception("E_MINE")), "E_MINE");
    }
}
