//! Program window: edit and compile stored PL/SQL (spec and body as tabs).

use std::collections::HashMap;

use egui::{RichText, Ui};
use pythia_db::{CompileError, DbEvent, DbRequest, MetaQuery, MetaResult, ObjectType};
use serde::{Deserialize, Serialize};

use crate::documents::{DocContext, Effects, ObjectRef, Target, request_id};
use crate::editor::{Editor, Marker, MarkerKind};

#[derive(Serialize, Deserialize)]
pub struct ProgramWindow {
    pub object: ObjectRef,
    pub parts: Vec<Part>,
    pub active: usize,
    #[serde(default = "yes")]
    pub show_outline: bool,
    /// Not in the database yet: compiling creates it.
    #[serde(default)]
    pub new: bool,
    #[serde(skip)]
    state: State,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
pub struct Part {
    pub object_type: ObjectType,
    pub editor: Editor,
    /// Text as last loaded or compiled, to detect unsaved changes.
    pub saved: String,
    #[serde(skip)]
    pub errors: Vec<CompileError>,
    #[serde(skip)]
    assist: crate::assist::Assist,
}

#[derive(Default)]
struct State {
    /// Source requests in flight, by part index.
    loading: HashMap<u64, usize>,
    /// Compile requests in flight: request id → part index.
    compiling: HashMap<u64, usize>,
    /// Parts still to compile after the current one (compile all).
    compile_queue: Vec<usize>,
    /// Compile with debug information (Settings → Debugger).
    debug_info: bool,
    /// The task turning debug information on before a compile, and why it
    /// failed, if it did.
    debug_setup: Option<u64>,
    debug_error: Option<String>,
    message: Option<(bool, String)>,
    /// Line to show once the part's source has loaded: (part, line).
    pending_goto: Option<(usize, usize)>,
    /// Subprogram (and parameter) to show once all parts have loaded.
    pending_member: Option<(String, Option<String>)>,
    /// LAST_DDL_TIME of each part when loaded or last compiled here.
    ddl_times: HashMap<usize, Option<String>>,
    /// Requests reading LAST_DDL_TIME: id → (part, purpose).
    ddl_requests: HashMap<u64, (usize, DdlCheck)>,
    /// The part was changed in the database by someone else: (part, time).
    conflict: Option<(usize, String)>,
    /// Concurrent-change checks still running before a compile.
    checks_pending: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DdlCheck {
    /// Remember the time (after load or compile).
    Remember,
    /// Compare before compiling.
    BeforeCompile,
}

impl ProgramWindow {
    pub fn new(object: ObjectRef) -> Self {
        let spec_type = match object.object_type {
            ObjectType::PackageBody => ObjectType::Package,
            ObjectType::TypeBody => ObjectType::Type,
            other => other,
        };
        let mut parts = vec![Part::new(spec_type)];
        if let Some(body) = spec_type.body() {
            parts.push(Part::new(body));
        }
        let active = parts
            .iter()
            .position(|p| p.object_type == object.object_type)
            .unwrap_or(0);
        Self {
            show_outline: true,
            object: ObjectRef {
                object_type: spec_type,
                ..object
            },
            parts,
            active,
            new: false,
            state: State::default(),
        }
    }

    /// A unit that doesn't exist yet, with its (unsaved) sources, spec
    /// first. Compiling creates it.
    pub fn with_sources(object: ObjectRef, sources: Vec<String>) -> Self {
        let mut window = Self::new(object);
        for (part, text) in window.parts.iter_mut().zip(sources) {
            part.editor.set_text(text);
        }
        window.active = 0;
        window.new = true;
        window
    }

    /// Shows `line` of the part with `object_type` (now, or once loaded).
    pub fn goto_line(&mut self, object_type: ObjectType, line: usize) {
        let Some(i) = self.parts.iter().position(|p| p.object_type == object_type) else {
            return;
        };
        self.active = i;
        if self.state.loading.values().any(|p| *p == i) {
            self.state.pending_goto = Some((i, line));
        } else {
            let part = &mut self.parts[i];
            part.editor.goto(line, 1);
            if part.is_dirty() {
                // Lines from the database (error stacks, the profiler, source
                // search) may have moved.
                self.state.message = Some((
                    false,
                    format!("Line {line} in the database: this part has unsaved changes"),
                ));
            }
        }
    }

    /// Shows a subprogram: its implementation in the body, else its
    /// declaration in the spec (now, or once the source has loaded).
    pub fn goto_member(&mut self, member: &str, parameter: Option<&str>) {
        self.state.pending_member = Some((member.to_owned(), parameter.map(str::to_owned)));
        if self.state.loading.is_empty() {
            self.show_member();
        }
    }

    fn show_member(&mut self) {
        let Some((member, parameter)) = self.state.pending_member.take() else {
            return;
        };
        // Bodies come after specs; look there first.
        for i in (0..self.parts.len()).rev() {
            let part = &mut self.parts[i];
            if let Some(span) = member_span(part.editor.text(), &member, parameter.as_deref()) {
                self.active = i;
                part.editor.select(span);
                return;
            }
        }
        self.state.message = Some((false, format!("{} not found", member.to_lowercase())));
    }

    pub fn active_editor(&mut self) -> Option<&mut Editor> {
        self.parts.get_mut(self.active).map(|p| &mut p.editor)
    }

    /// Toggles a breakpoint on the cursor line of the active part.
    pub fn toggle_breakpoint(&self) -> Effects {
        let mut effects = Effects::default();
        effects.breakpoints.extend(self.cursor_line());
        effects
    }

    /// The debugger's unit of the active part, and the cursor line in it.
    pub fn cursor_line(&self) -> Option<(pythia_debugger::Unit, u32)> {
        let part = self.parts.get(self.active)?;
        let line = part.editor.line_col().0 as u32;
        Some((debug_unit(&self.object, part.object_type), line))
    }

    /// File name for saving the active part, e.g. `emp_pkg.pkb`.
    pub fn file_name(&self) -> String {
        let ext = match self.parts.get(self.active).map(|p| p.object_type) {
            Some(ObjectType::Package) => "pks",
            Some(ObjectType::PackageBody) => "pkb",
            Some(ObjectType::Procedure) => "prc",
            Some(ObjectType::Function) => "fnc",
            Some(ObjectType::Type) => "tps",
            Some(ObjectType::TypeBody) => "tpb",
            Some(ObjectType::Trigger) => "trg",
            _ => "sql",
        };
        format!("{}.{ext}", self.object.name.to_lowercase())
    }

    /// Source of the active part, terminated for SQL*Plus.
    pub fn active_source(&self) -> String {
        let text = self
            .parts
            .get(self.active)
            .map_or("", |p| p.editor.text())
            .trim_end();
        format!("{text}\n/\n")
    }

    pub fn title(&self) -> String {
        self.object.name.to_lowercase()
    }

    pub fn is_busy(&self) -> bool {
        !self.state.loading.is_empty()
            || !self.state.compiling.is_empty()
            || self.state.checks_pending > 0
    }

    pub fn is_dirty(&self) -> bool {
        self.parts.iter().any(Part::is_dirty)
    }

    pub fn line_col(&self) -> (usize, usize) {
        self.parts
            .get(self.active)
            .map_or((1, 1), |p| p.editor.line_col())
    }

    /// Loads the source of all parts. Unsaved edits are kept.
    pub fn load(&mut self) -> Effects {
        let mut effects = Effects::default();
        if self.is_dirty() {
            return effects;
        }
        self.state.conflict = None;
        for i in 0..self.parts.len() {
            effects.extend(self.ddl_time(i, DdlCheck::Remember));
        }
        for (i, part) in self.parts.iter().enumerate() {
            let id = request_id();
            self.state.loading.insert(id, i);
            effects.request(
                Target::Main,
                DbRequest::Meta {
                    id,
                    query: MetaQuery::Source {
                        owner: self.object.owner.clone(),
                        name: self.object.name.clone(),
                        object_type: part.object_type,
                    },
                },
            );
        }
        effects
    }

    /// Formats the parts [`ProgramWindow::compile`] would compile (format
    /// on save).
    pub fn format_before_compile(&mut self, all: bool, options: &pythia_plsql::FormatOptions) {
        let active = self.active;
        for (i, part) in self.parts.iter_mut().enumerate() {
            if all || i == active {
                part.editor.format_all(options);
            }
        }
    }

    /// Compiles the active part, or all parts (spec first), with debug
    /// information if `debug_info`.
    pub fn compile(&mut self, all: bool, debug_info: bool) -> Effects {
        if self.is_busy() {
            return Effects::default();
        }
        self.state.debug_info = debug_info;
        self.state.compile_queue = if all {
            (0..self.parts.len()).rev().collect()
        } else {
            vec![self.active]
        };
        self.state.message = None;
        self.state.conflict = None;
        // Check every part for changes by others before compiling anything
        // (compiling the spec touches the body's DDL time).
        let known: Vec<usize> = self
            .state
            .compile_queue
            .iter()
            .copied()
            .filter(|i| self.state.ddl_times.get(i).is_some_and(Option::is_some))
            .collect();
        if known.is_empty() {
            return self.compile_part();
        }
        let mut effects = Effects::default();
        self.state.checks_pending = known.len();
        for part in known {
            effects.extend(self.ddl_time(part, DdlCheck::BeforeCompile));
        }
        effects
    }

    #[cfg(test)]
    pub fn has_conflict(&self) -> bool {
        self.state.conflict.is_some()
    }

    #[cfg(test)]
    pub fn message(&self) -> Option<(bool, String)> {
        self.state.message.clone()
    }

    /// Compiles despite a detected concurrent change.
    pub fn overwrite(&mut self) -> Effects {
        self.state.conflict = None;
        self.compile_part()
    }

    /// Reads the part's LAST_DDL_TIME from the database.
    fn ddl_time(&mut self, part: usize, check: DdlCheck) -> Effects {
        let mut effects = Effects::default();
        let (owner, name) = (self.object.owner.clone(), self.object.name.clone());
        let object_type = self.parts[part].object_type.dictionary_name();
        let id = request_id();
        self.state.ddl_requests.insert(id, (part, check));
        let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
            let time = conn
                .query_row_as::<Option<String>>(
                    "select max(to_char(last_ddl_time, 'YYYY-MM-DD HH24:MI:SS')) from all_objects \
                      where owner = :1 and object_name = :2 and object_type = :3",
                    &[&owner, &name, &object_type],
                )
                .map_err(pythia_db::DbError::from)?;
            Ok(Box::new(time) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Main, DbRequest::Task { id, task });
        effects
    }

    /// Compiles the next queued part without checking.
    fn compile_part(&mut self) -> Effects {
        let mut effects = Effects::default();
        while let Some(i) = self.state.compile_queue.pop() {
            let sql = runnable(self.parts[i].editor.text());
            if sql.trim().is_empty() {
                continue;
            }
            let id = request_id();
            self.state.compiling.insert(id, i);
            let execute = DbRequest::Execute {
                id,
                sql,
                max_rows: 1,
                binds: Vec::new(),
            };
            if self.state.debug_info {
                self.debug_compile(execute, &mut effects);
            } else {
                effects.request(Target::Own, execute);
            }
            break;
        }
        effects
    }

    /// Runs the `CREATE` with `PLSQL_DEBUG` on, then restores the session's
    /// own setting.
    fn debug_compile(&mut self, execute: DbRequest, effects: &mut Effects) {
        use pythia_debugger::target::set_compile_debug;
        type Previous = std::sync::Arc<std::sync::Mutex<Option<bool>>>;
        let previous: Previous = Default::default();
        let setup = request_id();
        self.state.debug_setup = Some(setup);
        self.state.debug_error = None;
        let remember = previous.clone();
        let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
            let was = set_compile_debug(conn, true)?;
            if let Ok(mut previous) = remember.lock() {
                *previous = Some(was);
            }
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id: setup, task });
        effects.request(Target::Own, execute);
        let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
            let was = previous.lock().ok().and_then(|mut p| p.take());
            if was == Some(false) {
                set_compile_debug(conn, false)?;
            }
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        });
        effects.request(
            Target::Own,
            DbRequest::Task {
                id: request_id(),
                task,
            },
        );
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        if let DbEvent::TaskDone { id, result } = &event
            && self.state.debug_setup == Some(*id)
        {
            self.state.debug_setup = None;
            self.state.debug_error = result.as_ref().err().map(|e| e.to_string());
            return Effects::default();
        }
        if let DbEvent::TaskDone { id, result } = &event
            && let Some((part, check)) = self.state.ddl_requests.remove(id)
        {
            let now: Option<String> = match result {
                Ok(any) => any.downcast_ref::<Option<String>>().cloned().flatten(),
                Err(_) => None,
            };
            match check {
                DdlCheck::Remember => {
                    self.state.ddl_times.insert(part, now);
                }
                DdlCheck::BeforeCompile => {
                    self.state.checks_pending = self.state.checks_pending.saturating_sub(1);
                    let known = self.state.ddl_times.get(&part).cloned().flatten();
                    if let (Some(known), Some(now)) = (known, now)
                        && now > known
                        && self.state.conflict.is_none()
                    {
                        self.active = part;
                        self.state.conflict = Some((part, now));
                    }
                    if self.state.checks_pending == 0 && self.state.conflict.is_none() {
                        return self.compile_part();
                    }
                }
            }
            return Effects::default();
        }
        match event {
            DbEvent::Meta { id, result } => {
                if let Some(i) = self.state.loading.remove(&id) {
                    match result {
                        Ok(MetaResult::Text(text)) => {
                            let part = &mut self.parts[i];
                            part.editor.set_text(text.clone());
                            part.saved = text;
                            if let Some((p, line)) = self.state.pending_goto
                                && p == i
                            {
                                self.state.pending_goto = None;
                                part.editor.goto(line, 1);
                            }
                        }
                        // A package without a body yet: offer an empty one.
                        Err(_) if i > 0 => {}
                        Err(e) => self.state.message = Some((true, e.to_string())),
                        Ok(_) => {}
                    }
                    if self.state.loading.is_empty() {
                        self.show_member();
                    }
                }
            }
            DbEvent::Executed {
                id, mut summary, ..
            } => {
                if let Some(i) = self.state.compiling.remove(&id) {
                    let debug_info = self.state.debug_info;
                    if debug_info && let Some(compiled) = &mut summary.compiled {
                        // Oracle's notes about the debug option itself.
                        compiled
                            .errors
                            .retain(|e| !pythia_debugger::target::is_debug_notice(&e.text));
                    }
                    let part = &mut self.parts[i];
                    part.saved = part.editor.text().to_owned();
                    part.errors = summary
                        .compiled
                        .as_ref()
                        .map(|c| c.errors.clone())
                        .unwrap_or_default();
                    part.editor.set_markers(markers(&part.errors));
                    let failed = part.errors.iter().any(|e| !e.warning);
                    let mut message = summary.to_string();
                    if debug_info && !failed {
                        match self.state.debug_error.take() {
                            None => message.push_str(" (with debug information)"),
                            Some(e) => {
                                message.push_str(&format!(" (without debug information: {e})"));
                            }
                        }
                    }
                    self.state.message = Some((failed, message));
                    if failed {
                        self.active = i;
                        self.state.compile_queue.clear();
                    }
                    let mut effects = self.ddl_time(i, DdlCheck::Remember);
                    // Valid or invalid now, and so may be its dependents.
                    effects.invalid_changed = true;
                    // Created, if with errors.
                    if self.new {
                        self.new = false;
                        effects.changed = Some(self.object.clone());
                    }
                    effects.extend(self.compile_part());
                    return effects;
                }
            }
            DbEvent::Failed { id, error, .. } => {
                if let Some(i) = self.state.compiling.remove(&id) {
                    self.active = i;
                    self.state.compile_queue.clear();
                    self.state.message = Some((true, error.to_string()));
                }
            }
            DbEvent::Error(error) | DbEvent::ConnectFailed(error) => {
                self.state.compiling.clear();
                self.state.message = Some((true, error.to_string()));
            }
            _ => {}
        }
        Effects::default()
    }

    /// The rows above the editor: the parts, Compile… and a conflict banner.
    fn toolbar_ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        ui.horizontal(|ui| {
            for (i, part) in self.parts.iter().enumerate() {
                let label = match part.object_type {
                    ObjectType::PackageBody | ObjectType::TypeBody => "Body",
                    ObjectType::Package | ObjectType::Type => "Specification",
                    _ => "Source",
                };
                let mut text = RichText::new(if part.is_dirty() {
                    format!("{label} •")
                } else {
                    label.to_owned()
                });
                if part.errors.iter().any(|e| !e.warning) {
                    text = text.color(ui.visuals().error_fg_color);
                }
                if ui.selectable_label(self.active == i, text).clicked() {
                    self.active = i;
                }
            }
            ui.separator();
            let busy = self.is_busy();
            if ui
                .add_enabled(!busy && cx.connected, egui::Button::new("Compile"))
                .on_hover_text("F8")
                .clicked()
            {
                if cx.settings.format_auto.on_save {
                    self.format_before_compile(false, &cx.settings.format);
                }
                effects.extend(self.compile(false, cx.settings.compile_debug_info));
            }
            if self.parts.len() > 1
                && ui
                    .add_enabled(!busy && cx.connected, egui::Button::new("Compile all"))
                    .on_hover_text("Compile the specification, then the body; stops at the first error")
                    .clicked()
            {
                if cx.settings.format_auto.on_save {
                    self.format_before_compile(true, &cx.settings.format);
                }
                effects.extend(self.compile(true, cx.settings.compile_debug_info));
            }
            if ui
                .add_enabled(!busy && cx.connected, egui::Button::new("Reload"))
                .on_hover_text("Discard changes and load the source from the database")
                .clicked()
            {
                for part in &mut self.parts {
                    part.saved = part.editor.text().to_owned();
                }
                effects.extend(self.load());
            }
            if self.parts.len() == 2
                && self.parts[0].object_type == ObjectType::Package
                && ui
                    .button("Generate body")
                    .on_hover_text("Create the body from the specification, or add stubs for missing subprograms")
                    .clicked()
            {
                let spec = self.parts[0].editor.text().to_owned();
                let body = self.parts[1].editor.text().to_owned();
                self.state.message = Some(match crate::stubs::package_body(&spec, Some(&body)) {
                    Some(stubs) if stubs.added > 0 => {
                        let len = body.len();
                        self.parts[1].editor.replace(0..len, &stubs.body, 1);
                        self.active = 1;
                        (false, format!("Added {} stub(s) to the body — compile to save", stubs.added))
                    }
                    Some(_) => (false, "The body already implements everything in the spec".into()),
                    None => (true, "Couldn't read the package specification".into()),
                });
            }
            if self.parts.len() == 2
                && self.parts[0].object_type == ObjectType::Package
                && ui
                    .button("Update spec")
                    .on_hover_text("Copy changed subprogram signatures from the body into the specification")
                    .clicked()
            {
                let spec = self.parts[0].editor.text().to_owned();
                let body = self.parts[1].editor.text().to_owned();
                self.state.message = Some(match crate::stubs::sync_spec(&spec, &body) {
                    Some(sync) if !sync.updated.is_empty() => {
                        let len = spec.len();
                        self.parts[0].editor.replace(0..len, &sync.spec, 1);
                        self.active = 0;
                        (
                            false,
                            format!(
                                "Updated {} in the spec — compile to save",
                                sync.updated.join(", ")
                            ),
                        )
                    }
                    Some(_) => (false, "The spec matches the body".into()),
                    None => (true, "Couldn't read the package specification".into()),
                });
            }
            ui.toggle_value(&mut self.show_outline, "Outline");
            if busy {
                ui.spinner();
            }
            if let Some((error, text)) = &self.state.message {
                let color = if *error {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().weak_text_color()
                };
                ui.label(RichText::new(text).color(color));
            }
        });

        if let Some((conflicted, time)) = self.state.conflict.clone() {
            let what = match self.parts[conflicted].object_type {
                ObjectType::PackageBody | ObjectType::TypeBody => "body",
                ObjectType::Package | ObjectType::Type => "specification",
                _ => "source",
            };
            let warn = ui.visuals().warn_fg_color;
            egui::Frame::new().fill(warn.gamma_multiply(0.15)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add(pythia_ui::icons::Icon::Warning.image(ui));
                    ui.label(
                        RichText::new(format!(
                            "The {what} was changed in the database at {time}, after you opened it. Compiling would overwrite those changes."
                        ))
                        .color(warn),
                    );
                    if ui.button("Overwrite").clicked() {
                        effects.extend(self.overwrite());
                    }
                    if ui.button("Reload from database").on_hover_text("Discards your changes to this part").clicked() {
                        self.state.conflict = None;
                        self.state.compile_queue.clear();
                        for part in &mut self.parts {
                            part.saved = part.editor.text().to_owned();
                        }
                        effects.extend(self.load());
                    }
                    if ui.button("Cancel").clicked() {
                        self.state.conflict = None;
                        self.state.compile_queue.clear();
                    }
                });
            });
            ui.add_space(4.0);
        }
        effects
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        // The tab has no margin, the toolbar panel brings its own.
        let mut effects = egui::Panel::top(egui::Id::new(("program-toolbar", cx.id)))
            .show(ui, |ui| self.toolbar_ui(ui, cx))
            .inner;
        // The other part of a package or type: its declarations are known
        // here, and jumps and references can lead there.
        let companion = self.companion_of(self.active);
        match companion {
            Some(other) => {
                let active = self.active;
                let (low, high) = self.parts.split_at_mut(active.max(other));
                let (this, that) = if other < active {
                    (&mut high[0], &low[other])
                } else {
                    (&mut low[active], &high[0])
                };
                this.assist
                    .set_companion(Some((that.editor.revision(), other < active)), || {
                        that.editor.text().to_owned()
                    });
            }
            None => {
                if let Some(part) = self.parts.get_mut(self.active) {
                    part.assist.set_companion(None, String::new);
                }
            }
        }
        let unsaved = self.is_dirty();
        // A package body's spec tells which of its subprograms are public
        // (Test… and Debug… in the context menus); analyze it although it
        // isn't shown.
        let (before, rest) = self.parts.split_at_mut(self.active);
        let Some(part) = rest.first_mut() else {
            return effects;
        };
        let spec = before.first_mut().map(|spec| {
            spec.assist.background(&mut spec.editor, ui, true);
            &spec.assist
        });
        if !part.errors.is_empty() {
            egui::Panel::bottom(egui::Id::new(("program-errors", cx.id)))
                .resizable(true)
                .default_size(110.0)
                .show(ui, |ui| {
                    if part.errors.iter().any(|e| !e.warning)
                        && crate::ai::button(
                            ui,
                            &cx.settings.ai,
                            "Fix with AI",
                            "Find the cause of the compile errors and suggest corrected code",
                        )
                    {
                        effects.commands.push(crate::commands::Command::AiFix);
                    }
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            let mut goto = None;
                            for e in &part.errors {
                                let color = if e.warning {
                                    ui.visuals().warn_fg_color
                                } else {
                                    ui.visuals().error_fg_color
                                };
                                let text = RichText::new(format!(
                                    "{:>5}:{:<4} {}",
                                    e.line, e.position, e.text
                                ))
                                .monospace()
                                .color(color);
                                if ui.link(text).clicked() {
                                    goto = Some((e.line as usize, e.position as usize));
                                }
                            }
                            if let Some((line, col)) = goto {
                                part.editor.goto(line, col);
                            }
                        });
                });
        }
        // From the outline's menu: references to find (at this offset), the
        // other part to go to (offset, name).
        let (mut find_references, mut go_to) = (None, None);
        if self.show_outline {
            egui::Panel::right(egui::Id::new(("program-outline", cx.id)))
                .resizable(true)
                .default_size(240.0)
                .show(ui, |ui| {
                    let mut outline = Outline {
                        object: &self.object,
                        part_type: part.object_type,
                        outline: part.assist.outline(),
                        spec: spec.map_or(part.assist.outline(), |s| s.outline()),
                        new: self.new,
                        unsaved,
                        other: companion.map(|_| match part.object_type {
                            ObjectType::Package | ObjectType::Type => "body",
                            _ => "specification",
                        }),
                        cx,
                        picked: None,
                        effects: Effects::default(),
                    };
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            if outline.outline.is_empty() {
                                ui.weak("No declarations");
                            }
                            for item in outline.outline {
                                outline.item(ui, item, None, 0);
                            }
                        });
                    effects.extend(outline.effects);
                    match outline.picked {
                        Some(Picked::Jump(range)) => part.editor.select(range),
                        Some(Picked::References(at)) => find_references = Some(at),
                        Some(Picked::Counterpart(at, name)) => go_to = Some((at, name)),
                        None => {}
                    }
                });
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let mut out = part.editor.ui(
                ui,
                ("program-editor", cx.id, self.active),
                &cx.settings.editor,
            );
            let unit = debug_unit(&self.object, part.object_type);
            // While debugging, hovering a variable shows its value.
            if let (Some(offset), Some(debug)) = (out.hover_request, cx.debug)
                && crate::editor::debug_hover(&mut part.editor, offset, debug, &unit)
            {
                out.hover_request = None;
            }
            effects.extend(part.assist.handle(&mut part.editor, &out, cx));
            let subprogram = part.assist.context().filter(|_| !self.new).and_then(|c| {
                let spec = spec.map_or(part.assist.outline(), |s| s.outline());
                let (object, member) = subprogram_at(
                    &self.object,
                    part.object_type,
                    part.assist.outline(),
                    spec,
                    c.offset,
                )?;
                Some(crate::editor_menu::Testable {
                    object,
                    member,
                    unsaved,
                })
            });
            effects.extend(crate::editor_menu::show(
                &out.response,
                &mut part.editor,
                &mut part.assist,
                subprogram,
                cx,
            ));
            part.editor
                .format_as_typed(&out, cx.settings.format_auto, &cx.settings.format);
            part.assist.background(&mut part.editor, ui, true);
            if let Some(line) = out.gutter_clicked {
                effects.breakpoints.push((unit.clone(), line as u32));
            }
            if let Some(debug) = cx.debug {
                let (lines, current) = debug.markers(&unit);
                part.editor
                    .set_debug_lines(&lines, current, &debug.sql_flags(Some(&unit)));
            }
        });
        // Looked up next frame, as from the editor's menu: the list of
        // references would take the click that chose this for one outside
        // it, and close.
        if let Some(at) = find_references {
            part.assist.request(crate::assist::Request::References, at);
            ui.ctx().request_repaint();
        }
        let (from, jump, references) = (
            part.assist.take_jumped_from(),
            part.assist.take_companion_jump(),
            part.assist.take_references(),
        );
        let active = self.active;
        if let Some(from) = from {
            effects.jumped_from = Some((active, from));
        }
        if let (Some(range), Some(other)) = (jump, companion) {
            self.active = other;
            self.parts[other].editor.select(range);
        }
        if let (Some((at, name)), Some(other)) = (go_to, companion) {
            let target = counterpart(
                self.parts[active].editor.text(),
                at,
                self.parts[other].editor.text(),
            );
            match target {
                Some(range) => {
                    effects.jumped_from = self.position();
                    self.active = other;
                    self.parts[other].editor.select(range);
                }
                None => {
                    let what = match self.parts[other].object_type {
                        ObjectType::PackageBody | ObjectType::TypeBody => "body",
                        _ => "specification",
                    };
                    self.state.message = Some((
                        false,
                        format!("{} isn't in the {what}", name.to_lowercase()),
                    ));
                }
            }
        }
        if let Some(places) = references {
            effects.references = Some(
                places
                    .into_iter()
                    .filter_map(|place| {
                        let (i, range) = match place {
                            crate::assist::Place::Here(r) => (active, r),
                            crate::assist::Place::Companion(r) => (companion?, r),
                        };
                        let part = &self.parts[i];
                        Some(crate::documents::Reference::new(
                            i,
                            range,
                            part.editor.text(),
                            Some(part.label()),
                        ))
                    })
                    .collect(),
            );
        }
        effects
    }

    /// Asks the active part's editor for navigation at its caret.
    pub fn navigate(&mut self, request: crate::assist::Request) {
        if let Some(part) = self.parts.get_mut(self.active) {
            let at = part.editor.cursor();
            part.assist.request(request, at);
        }
    }

    /// Declarations of all parts, for Go to Symbol.
    pub fn symbols(&self) -> Vec<crate::dialogs::SymbolItem> {
        let several = self.parts.len() > 1;
        self.parts
            .iter()
            .enumerate()
            .flat_map(|(i, p)| {
                crate::dialogs::symbol_items(i, p.editor.text(), several.then(|| p.label()))
            })
            .collect()
    }

    /// Shows the other part, at the counterpart of the subprogram around the
    /// caret. Returns where it jumped from.
    pub fn switch_part(&mut self) -> Option<(usize, usize)> {
        let other = self.companion_of(self.active)?;
        let from = self.position()?;
        let here = &self.parts[self.active];
        let target = counterpart(
            here.editor.text(),
            here.editor.cursor(),
            self.parts[other].editor.text(),
        );
        self.active = other;
        if let Some(span) = target {
            self.parts[other].editor.select(span);
        }
        Some(from)
    }

    /// The part a part's declarations pair with: spec ↔ body.
    fn companion_of(&self, part: usize) -> Option<usize> {
        (self.parts.len() == 2).then_some(1 - part.min(1))
    }

    /// Shows a place of a part (navigation history, reference lists).
    pub fn goto_position(&mut self, part: usize, range: std::ops::Range<usize>) {
        if let Some(p) = self.parts.get_mut(part) {
            self.active = part;
            p.editor.select(range);
        }
    }

    /// The active part and caret, for navigation history.
    pub fn position(&self) -> Option<(usize, usize)> {
        let part = self.parts.get(self.active)?;
        Some((self.active, part.editor.cursor()))
    }
}

impl Part {
    fn new(object_type: ObjectType) -> Self {
        Self {
            object_type,
            editor: Editor::default(),
            saved: String::new(),
            errors: Vec::new(),
            assist: Default::default(),
        }
    }

    /// "Spec" or "Body" (or "Source" for a single unit).
    fn label(&self) -> &'static str {
        match self.object_type {
            ObjectType::PackageBody | ObjectType::TypeBody => "Body",
            ObjectType::Package | ObjectType::Type => "Spec",
            _ => "Source",
        }
    }

    fn is_dirty(&self) -> bool {
        self.editor.text() != self.saved
    }
}

/// A procedure or function of a text (nested ones too).
struct SubprogramItem {
    /// Upper-cased.
    name: String,
    params: String,
    span: std::ops::Range<usize>,
    name_span: std::ops::Range<usize>,
}

/// The procedures and functions of a text, in order.
fn subprogram_items(text: &str) -> Vec<SubprogramItem> {
    use pythia_lsp_core::{OutlineItem, OutlineKind};
    fn walk(items: &[OutlineItem], out: &mut Vec<SubprogramItem>) {
        for item in items {
            if matches!(item.kind, OutlineKind::Procedure | OutlineKind::Function) {
                out.push(SubprogramItem {
                    name: item.name.to_uppercase(),
                    params: params(item),
                    span: item.span.clone(),
                    name_span: item.name_span.clone(),
                });
            }
            walk(&item.children, out);
        }
    }
    let mut out = Vec::new();
    walk(&pythia_lsp_core::outline(text), &mut out);
    out
}

/// A subprogram's parameters and result as the outline shows them, for
/// comparing a declaration with its implementation (case and spacing
/// ignored).
fn params(item: &pythia_lsp_core::OutlineItem) -> String {
    item.detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The subprograms around `offset`, outermost first, with which one of
/// that name each is.
fn enclosing_subprograms(text: &str, offset: usize) -> Vec<(SubprogramItem, usize)> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    subprogram_items(text)
        .into_iter()
        .filter_map(|item| {
            let count = seen.entry(item.name.clone()).or_default();
            *count += 1;
            let nth = *count - 1;
            (item.span.start <= offset && offset <= item.span.end).then_some((item, nth))
        })
        .collect()
}

/// The name in `there` of the counterpart of the innermost subprogram
/// around `offset` in `here` that has one (a local subprogram of a body
/// has none): the one of that name with the same parameters, else the one
/// at the same place among those of that name.
fn counterpart(here: &str, offset: usize, there: &str) -> Option<std::ops::Range<usize>> {
    let there = subprogram_items(there);
    enclosing_subprograms(here, offset)
        .into_iter()
        .rev()
        .find_map(|(item, nth)| {
            let same: Vec<_> = there.iter().filter(|t| t.name == item.name).collect();
            same.iter()
                .find(|t| t.params == item.params)
                .or(same.get(nth))
                .or(same.first())
                .map(|t| t.name_span.clone())
        })
}

/// The name of the declaration `member` (a subprogram, variable, type, …)
/// in `text`, or of its `parameter`: in the first overload that has it.
fn member_span(
    text: &str,
    member: &str,
    parameter: Option<&str>,
) -> Option<std::ops::Range<usize>> {
    use pythia_lsp_core::{OutlineItem, OutlineKind};
    fn collect(items: &[OutlineItem], member: &str, out: &mut Vec<(usize, usize, usize)>) {
        for item in items {
            let declaration = !matches!(item.kind, OutlineKind::Statement | OutlineKind::Block);
            if declaration && item.name.eq_ignore_ascii_case(member) {
                out.push((item.name_span.start, item.name_span.end, item.span.end));
            }
            collect(&item.children, member, out);
        }
    }
    let mut found = Vec::new();
    collect(&pythia_lsp_core::outline(text), member, &mut found);
    if let Some(parameter) = parameter {
        // The first mention after the name is in the parameter list.
        for &(_, name_end, end) in &found {
            if let Some(at) = find_word(&text[name_end..end.min(text.len())], parameter) {
                return Some(name_end + at..name_end + at + parameter.len());
            }
        }
    }
    found.first().map(|&(start, end, _)| start..end)
}

/// Byte offset of the first whole-word, case-insensitive `word` in `text`.
fn find_word(text: &str, word: &str) -> Option<usize> {
    let is_ident = |c: char| c.is_alphanumeric() || matches!(c, '_' | '$' | '#');
    let upper = text.to_uppercase();
    let word = word.to_uppercase();
    // Upper-casing keeps the byte offsets of ASCII identifiers.
    (upper.len() == text.len())
        .then(|| {
            upper.match_indices(&word).map(|(i, _)| i).find(|&i| {
                !text[..i].chars().next_back().is_some_and(is_ident)
                    && !text[i + word.len()..].chars().next().is_some_and(is_ident)
            })
        })
        .flatten()
}

/// The subprogram whose name is at `offset` in a part, for Test… and
/// Debug…: the standalone procedure or function, or a subprogram of a
/// package that its spec declares (`spec`; a block can't call private
/// ones). Gives the object and, for a package, the member with its
/// overload: Oracle numbers overloads in the order of the spec, and an
/// implementation in the body is the declaration with its parameters.
fn subprogram_at(
    object: &ObjectRef,
    part_type: ObjectType,
    outline: &[pythia_lsp_core::OutlineItem],
    spec: &[pythia_lsp_core::OutlineItem],
    offset: usize,
) -> Option<(ObjectRef, Option<crate::browser::Subprogram>)> {
    use pythia_lsp_core::{OutlineItem, OutlineKind as K};
    let subprogram = |item: &&OutlineItem| matches!(item.kind, K::Procedure | K::Function);
    let named = |item: &&OutlineItem| {
        subprogram(item) && item.name_span.start <= offset && offset <= item.name_span.end
    };
    // The subprograms declared at the top of a package spec or body.
    fn members(items: &[OutlineItem]) -> impl Iterator<Item = &OutlineItem> {
        items
            .iter()
            .filter(|i| matches!(i.kind, K::PackageSpec | K::PackageBody))
            .flat_map(|i| &i.children)
    }
    match part_type {
        ObjectType::Procedure | ObjectType::Function => {
            outline.iter().find(named).map(|_| (object.clone(), None))
        }
        ObjectType::Package | ObjectType::PackageBody => {
            let item = members(outline).find(named)?;
            let same = |i: &&OutlineItem| subprogram(i) && i.name.eq_ignore_ascii_case(&item.name);
            let declared: Vec<&OutlineItem> = members(spec).filter(same).collect();
            let index = if part_type == ObjectType::Package {
                declared.iter().position(|d| std::ptr::eq(*d, item))
            } else {
                // The same parameters, else the same position.
                declared
                    .iter()
                    .position(|d| params(d) == params(item))
                    .or_else(|| {
                        members(outline)
                            .filter(same)
                            .position(|i| std::ptr::eq(i, item))
                    })
            }
            .filter(|i| *i < declared.len())?;
            let package = ObjectRef {
                object_type: ObjectType::Package,
                ..object.clone()
            };
            let member = crate::browser::Subprogram {
                name: item.name.to_uppercase(),
                overload: (declared.len() > 1).then_some(index as u32 + 1),
            };
            Some((package, Some(member)))
        }
        _ => None,
    }
}

/// The debugger's name for a program window part.
pub fn debug_unit(object: &ObjectRef, object_type: ObjectType) -> pythia_debugger::Unit {
    use pythia_debugger::UnitType;
    let unit_type = match object_type {
        ObjectType::Package => UnitType::Package,
        ObjectType::PackageBody => UnitType::PackageBody,
        ObjectType::Function => UnitType::Function,
        ObjectType::Trigger => UnitType::Trigger,
        ObjectType::Type => UnitType::Type,
        ObjectType::TypeBody => UnitType::TypeBody,
        _ => UnitType::Procedure,
    };
    pythia_debugger::Unit::new(object.owner.clone(), object.name.clone(), unit_type)
}

/// The outline beside the editor: its entries jump to their declaration,
/// and their context menu tests, debugs, finds and copies.
struct Outline<'a> {
    object: &'a ObjectRef,
    part_type: ObjectType,
    /// This part's declarations, and those of the package spec (the same
    /// in the spec), for Test… and Debug….
    outline: &'a [pythia_lsp_core::OutlineItem],
    spec: &'a [pythia_lsp_core::OutlineItem],
    new: bool,
    unsaved: bool,
    /// The other part, for Go to: "body" or "specification".
    other: Option<&'static str>,
    cx: &'a DocContext<'a>,
    picked: Option<Picked>,
    effects: Effects,
}

/// What was picked in the outline, for the window to carry out.
enum Picked {
    /// Select the name.
    Jump(std::ops::Range<usize>),
    /// Find references to the name at this offset.
    References(usize),
    /// Show the subprogram named at this offset in the other part.
    Counterpart(usize, String),
}

impl Outline<'_> {
    /// One entry with its children, indented by depth.
    fn item(
        &mut self,
        ui: &mut Ui,
        item: &pythia_lsp_core::OutlineItem,
        parent: Option<pythia_lsp_core::OutlineKind>,
        depth: usize,
    ) {
        use pythia_lsp_core::OutlineKind as K;
        use pythia_ui::icons::{Icon, IconSize};
        let icon = match item.kind {
            K::PackageSpec => Icon::Package,
            K::PackageBody => Icon::PackageBody,
            K::Procedure => Icon::Procedure,
            K::Function => Icon::Function,
            K::Trigger => Icon::Trigger,
            K::Type | K::TypeDecl => Icon::Type,
            K::TypeBody => Icon::TypeBody,
            K::Cursor => Icon::Grid,
            K::Exception => Icon::Warning,
            K::Variable | K::Constant => Icon::Column,
            K::Block | K::Statement => Icon::Text,
        };
        // Variables of nested scopes would only add noise.
        if depth > 1
            && matches!(
                item.kind,
                K::Variable | K::Constant | K::Statement | K::Block
            )
        {
            return;
        }
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 14.0);
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.add(icon.sized(ui, IconSize::Small));
            let mut r = ui.add(
                egui::Button::new(RichText::new(item.name.to_lowercase()).monospace()).frame(false),
            );
            if r.clicked() {
                self.picked = Some(Picked::Jump(item.name_span.clone()));
            }
            if !item.detail.is_empty() {
                r = r.on_hover_text(&item.detail);
            }
            // Blocks and statements have no name.
            if !matches!(item.kind, K::Block | K::Statement) {
                r.context_menu(|ui| self.menu(ui, item, parent));
            }
        });
        for child in &item.children {
            self.item(ui, child, Some(item.kind), depth + 1);
        }
    }

    fn menu(
        &mut self,
        ui: &mut Ui,
        item: &pythia_lsp_core::OutlineItem,
        parent: Option<pythia_lsp_core::OutlineKind>,
    ) {
        use pythia_lsp_core::OutlineKind as K;
        ui.set_min_width(180.0);
        let subprogram = matches!(item.kind, K::Procedure | K::Function);
        if subprogram {
            let testable = self.testable(item, parent);
            let testable = testable.as_ref().map_err(|why| *why);
            if crate::editor_menu::test_items(ui, testable, self.cx, &mut self.effects) {
                ui.separator();
            }
        }
        // A package's or type's own subprogram, not a local one.
        let member = subprogram
            && matches!(
                parent,
                Some(K::PackageSpec | K::PackageBody | K::Type | K::TypeBody)
            );
        if member
            && let Some(other) = self.other
            && ui.button(format!("Go to {other}")).clicked()
        {
            self.picked = Some(Picked::Counterpart(item.name_span.start, item.name.clone()));
            ui.close();
        }
        if ui.button("Find references").clicked() {
            self.picked = Some(Picked::References(item.name_span.start));
            ui.close();
        }
        ui.separator();
        if ui.button("Copy name").clicked() {
            ui.ctx().copy_text(item.name.clone());
            ui.close();
        }
    }

    /// The subprogram to test or debug, or why a block can't call it.
    fn testable(
        &self,
        item: &pythia_lsp_core::OutlineItem,
        parent: Option<pythia_lsp_core::OutlineKind>,
    ) -> Result<crate::editor_menu::Testable, &'static str> {
        use pythia_lsp_core::OutlineKind as K;
        if self.new {
            return Err("Not in the database yet: compile it first");
        }
        let offset = item.name_span.start;
        match subprogram_at(self.object, self.part_type, self.outline, self.spec, offset) {
            Some((object, member)) => Ok(crate::editor_menu::Testable {
                object,
                member,
                unsaved: self.unsaved,
            }),
            None => Err(match parent {
                Some(K::PackageBody) => "Private: the package specification doesn't declare it",
                Some(K::Type | K::TypeBody) => "Methods of object types can't be tested",
                _ => "A local subprogram: it can only be called where it's declared",
            }),
        }
    }
}

/// The part's text as a single statement: drops a trailing `/` line.
fn runnable(text: &str) -> String {
    let trimmed = text.trim_end();
    let trimmed = trimmed
        .strip_suffix('/')
        .filter(|t| t.ends_with('\n'))
        .unwrap_or(trimmed);
    trimmed.trim_end().to_owned()
}

fn markers(errors: &[CompileError]) -> Vec<Marker> {
    errors
        .iter()
        .map(|e| Marker {
            line: e.line as usize,
            kind: if e.warning {
                MarkerKind::Warning
            } else {
                MarkerKind::Error
            },
            message: e.text.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{enclosing_subprograms, member_span, subprogram_at, subprogram_items};
    use crate::browser::Subprogram;
    use crate::documents::ObjectRef;
    use pythia_db::ObjectType;

    /// Test… and Debug… on a subprogram's name: in the spec, in the body
    /// when the spec declares it, and for a standalone function.
    #[test]
    fn subprogram_under_the_right_click() {
        let spec = "create or replace package hr.emp_pkg as\n  procedure raise_salary(p_id number);\n  \
                    g_rate number;\nend;";
        let body = "create or replace package body hr.emp_pkg as\n  \
                    procedure helper is begin null; end;\n  \
                    procedure raise_salary(p_id number) is\n  begin\n    helper;\n  end;\nend;";
        let (spec_outline, body_outline) = (
            pythia_lsp_core::outline(spec),
            pythia_lsp_core::outline(body),
        );
        let package = ObjectRef {
            owner: "HR".into(),
            name: "EMP_PKG".into(),
            object_type: ObjectType::Package,
        };
        let at = |text: &str, word: &str| text.find(word).unwrap() + 2;
        let member = Some((
            package.clone(),
            Some(Subprogram {
                name: "RAISE_SALARY".into(),
                overload: None,
            }),
        ));
        let spec_at = |offset| {
            subprogram_at(
                &package,
                ObjectType::Package,
                &spec_outline,
                &spec_outline,
                offset,
            )
        };
        let body_at = |offset| {
            subprogram_at(
                &package,
                ObjectType::PackageBody,
                &body_outline,
                &spec_outline,
                offset,
            )
        };
        assert_eq!(spec_at(at(spec, "raise_salary")), member);
        // The end of the name counts (a click on its last letter's right half).
        let end = spec.find("raise_salary").unwrap() + "raise_salary".len();
        assert_eq!(spec_at(end), member);
        assert_eq!(spec_at(at(spec, "p_id")), None);
        assert_eq!(spec_at(at(spec, "g_rate")), None);
        assert_eq!(body_at(at(body, "raise_salary")), member);
        assert_eq!(body_at(at(body, "helper")), None, "private");
        assert_eq!(body_at(at(body, "helper;")), None, "a call");
        // Without the spec's outline the body offers nothing.
        assert_eq!(
            subprogram_at(
                &package,
                ObjectType::PackageBody,
                &body_outline,
                &[],
                at(body, "raise_salary")
            ),
            None
        );

        // Overloads: numbered in the spec's order; in the body by their
        // parameters, whatever the order there.
        let spec = "create or replace package hr.emp_pkg as\n  procedure p(p_b varchar2);\n  \
                    procedure q;\n  procedure p(p_a number, p_c date);\nend;";
        let body = "create or replace package body hr.emp_pkg as\n  \
                    procedure p(P_A  NUMBER, p_c date) is begin null; end;\n  \
                    procedure q is begin null; end;\n  \
                    procedure p(p_b varchar2) is begin null; end;\nend;";
        let (spec_outline, body_outline) = (
            pythia_lsp_core::outline(spec),
            pythia_lsp_core::outline(body),
        );
        let overload = |part, outline, text: &str, from: &str| {
            let offset = text.find(from).unwrap() + "procedure ".len();
            subprogram_at(&package, part, outline, &spec_outline, offset)
                .and_then(|(_, m)| m)
                .map(|m| (m.name, m.overload))
        };
        let p = |n| Some(("P".to_owned(), Some(n)));
        assert_eq!(
            overload(ObjectType::Package, &spec_outline, spec, "procedure p(p_b"),
            p(1)
        );
        assert_eq!(
            overload(ObjectType::Package, &spec_outline, spec, "procedure p(p_a"),
            p(2)
        );
        assert_eq!(
            overload(ObjectType::Package, &spec_outline, spec, "procedure q"),
            Some(("Q".to_owned(), None)),
            "not overloaded"
        );
        assert_eq!(
            overload(
                ObjectType::PackageBody,
                &body_outline,
                body,
                "procedure p(P_A"
            ),
            p(2)
        );
        assert_eq!(
            overload(
                ObjectType::PackageBody,
                &body_outline,
                body,
                "procedure p(p_b"
            ),
            p(1)
        );

        let function = ObjectRef {
            owner: "HR".into(),
            name: "TWICE".into(),
            object_type: ObjectType::Function,
        };
        let text = "create or replace function twice(p_n number) return number is\nbegin\n  return p_n * 2;\nend;";
        let outline = pythia_lsp_core::outline(text);
        assert_eq!(
            subprogram_at(
                &function,
                ObjectType::Function,
                &outline,
                &[],
                at(text, "twice")
            ),
            Some((function.clone(), None))
        );
        assert_eq!(
            subprogram_at(
                &function,
                ObjectType::Function,
                &outline,
                &[],
                at(text, "return p_n")
            ),
            None
        );
    }

    #[test]
    fn subprograms_around_the_caret() {
        let body = "create or replace package body p as\n  \
                    procedure a(x number) is\n    procedure inner is begin null; end;\n  \
                    begin inner; end;\n  procedure a is begin null; end;\nend;";
        let around = |at| -> Vec<(String, usize)> {
            enclosing_subprograms(body, at)
                .into_iter()
                .map(|(item, nth)| (item.name, nth))
                .collect()
        };
        let at = body.find("begin inner").unwrap();
        assert_eq!(around(at), [("A".to_owned(), 0)]);
        let at = body.find("begin null; end;\nend").unwrap();
        assert_eq!(around(at), [("A".to_owned(), 1)]);
        let at = body.find("null; end;\n  begin").unwrap();
        assert_eq!(around(at), [("A".to_owned(), 0), ("INNER".to_owned(), 0)]);
        let names: Vec<String> = subprogram_items(body).into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["A", "INNER", "A"]);
    }

    /// Switch Spec/Body lands on the overload with the same parameters,
    /// whatever order the body implements them in.
    #[test]
    fn switch_to_the_same_overload() {
        let spec = "create or replace package p as\n  procedure a(p_x number);\n  \
                    procedure a(p_y date);\nend;";
        let body = "create or replace package body p as\n  \
                    procedure a(p_y date) is begin null; end;\n  \
                    procedure a(p_x number) is begin null; end;\nend;";
        let mut w = ProgramWindow::new(ObjectRef {
            owner: "HR".into(),
            name: "P".into(),
            object_type: ObjectType::PackageBody,
        });
        for (part, text) in w.parts.iter_mut().zip([spec, body]) {
            part.editor.set_text(text);
        }
        let selected = |w: &mut ProgramWindow| (w.active, w.active_editor().unwrap().selection());
        let x = body.find("a(p_x").unwrap();
        w.goto_position(1, x..x);
        w.switch_part();
        let in_spec = spec.find("a(p_x").unwrap();
        assert_eq!(selected(&mut w), (0, Some(in_spec..in_spec + 1)));
        w.switch_part();
        assert_eq!(selected(&mut w), (1, Some(x..x + 1)));
        let y = spec.find("a(p_y").unwrap();
        w.goto_position(0, y..y);
        w.switch_part();
        let in_body = body.find("a(p_y").unwrap();
        assert_eq!(selected(&mut w), (1, Some(in_body..in_body + 1)));
    }

    #[test]
    fn members_and_their_parameters() {
        let body = "create or replace package body p as\n  g_x number;\n  \
                    procedure raise(p_id number, p_pct number) is\n  begin null; end;\n  \
                    procedure raise(p_id number, p_amount number) is\n  begin null; end;\nend;";
        let first = body.find("raise").unwrap();
        assert_eq!(member_span(body, "RAISE", None), Some(first..first + 5));
        // The overload that has the parameter.
        let amount = body.find("p_amount").unwrap();
        assert_eq!(
            member_span(body, "raise", Some("P_AMOUNT")),
            Some(amount..amount + 8)
        );
        let x = body.find("g_x").unwrap();
        assert_eq!(member_span(body, "G_X", None), Some(x..x + 3));
        assert_eq!(member_span(body, "nope", None), None);
    }

    use super::*;

    #[test]
    fn strips_slash() {
        assert_eq!(
            runnable("create package p as end;\n/\n"),
            "create package p as end;"
        );
        assert_eq!(runnable("x := 4/\n"), "x := 4/");
    }

    #[test]
    fn spec_and_body_parts() {
        let w = ProgramWindow::new(ObjectRef {
            owner: "HR".into(),
            name: "PKG".into(),
            object_type: ObjectType::PackageBody,
        });
        assert_eq!(w.parts.len(), 2);
        assert_eq!(w.active, 1);
        assert_eq!(w.object.object_type, ObjectType::Package);
    }
}
