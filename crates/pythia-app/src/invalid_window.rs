//! Invalid objects: the list with their first error, and recompiling them in
//! dependency order over several sessions, with progress and cancel.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{RichText, Ui};
use egui_extras::{Column as TableColumn, TableBuilder};
use pythia_db::oracle::Connection;
use pythia_db::recompile::{
    InvalidObject, InvalidScope, ObjectId, Progress, RecompileSummary, invalid_objects,
};
use pythia_db::{DbEvent, DbRequest, Dialect, ObjectType};
use pythia_ui::icons::Icon;
use serde::{Deserialize, Serialize};

use crate::documents::{DocContext, Effects, ObjectRef, OpenRequest, Target, request_id};

#[derive(Default, Serialize, Deserialize)]
pub struct InvalidObjectsWindow {
    /// The schema shown, or every user schema.
    pub schema: Option<String>,
    pub object_type: Option<String>,
    pub filter: String,
    /// Recompile everything once loaded (the browser's button).
    #[serde(skip)]
    pub recompile_on_load: bool,
    #[serde(skip)]
    state: State,
}

#[derive(Default)]
struct State {
    loaded: bool,
    objects: Vec<InvalidObject>,
    /// Invalid before the last recompile, to tell what it fixed.
    before: BTreeSet<ObjectId>,
    selected: BTreeSet<ObjectId>,
    pending: BTreeMap<u64, Pending>,
    run: Option<Run>,
    /// Compile everything with UTL_RECOMP (SYSDBA).
    utl_recomp: bool,
    result: Option<String>,
    error: Option<String>,
}

enum Pending {
    Load,
    Recompile,
}

/// A recompile in progress.
struct Run {
    progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

/// A recompile for the app to run on the window's session, with worker
/// sessions of the same connection.
pub struct RecompileRequest {
    pub id: u64,
    pub objects: Vec<ObjectId>,
    pub parallelism: usize,
    pub progress: Arc<Mutex<Progress>>,
    pub cancel: Arc<AtomicBool>,
    /// Schemas to recompile with UTL_RECOMP instead.
    pub utl_recomp: Option<Vec<String>>,
}

/// What the recompile task hands back.
pub struct RecompileOutcome {
    pub summary: RecompileSummary,
    /// How it was done, when not the built-in way.
    pub note: Option<String>,
}

/// Runs a recompile request; `params` open the worker sessions.
pub fn recompile_task(
    request: RecompileRequest,
    params: pythia_db::ConnectParams,
) -> pythia_db::TaskFn {
    Box::new(move |conn: &Connection| {
        let mut note = None;
        if let Some(owners) = &request.utl_recomp {
            let started = Instant::now();
            let done = owners.iter().try_for_each(|owner| {
                pythia_db::recompile::recompile_with_utl_recomp(conn, owner, request.parallelism)
            });
            match done {
                Ok(()) => {
                    let summary = RecompileSummary {
                        compiled: request.objects.len(),
                        elapsed: started.elapsed(),
                        ..RecompileSummary::default()
                    };
                    return Ok(Box::new(RecompileOutcome {
                        summary,
                        note: Some("with UTL_RECOMP".into()),
                    }) as Box<dyn std::any::Any + Send>);
                }
                Err(e) => note = Some(format!("UTL_RECOMP failed ({e}), compiled one by one")),
            }
        }
        let summary = pythia_db::recompile::recompile(
            conn,
            &params,
            &request.objects,
            request.parallelism,
            &request.progress,
            &request.cancel,
        )?;
        Ok(Box::new(RecompileOutcome { summary, note }) as Box<dyn std::any::Any + Send>)
    })
}

impl InvalidObjectsWindow {
    pub fn new(schema: Option<String>, recompile: bool) -> Self {
        Self {
            schema,
            recompile_on_load: recompile,
            ..Self::default()
        }
    }

    pub fn title(&self) -> String {
        match &self.schema {
            Some(s) => format!("Invalid objects ({})", as_shown(s)),
            None => "Invalid objects".into(),
        }
    }

    pub fn is_busy(&self) -> bool {
        !self.state.pending.is_empty()
    }

    #[cfg(test)]
    pub fn objects(&self) -> &[InvalidObject] {
        &self.state.objects
    }

    #[cfg(test)]
    pub fn result(&self) -> Option<&str> {
        self.state.result.as_deref()
    }

    fn scope(&self) -> InvalidScope {
        match &self.schema {
            Some(s) => InvalidScope::Schema(s.clone()),
            None => InvalidScope::UserSchemas,
        }
    }

    pub fn refresh(&mut self) -> Effects {
        let mut effects = Effects::default();
        let id = request_id();
        let scope = self.scope();
        self.state.pending.insert(id, Pending::Load);
        self.state.loaded = true;
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            Ok(Box::new(invalid_objects(conn, &scope)?) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    /// Starts recompiling `objects` (those that may be compiled).
    fn recompile(&mut self, objects: Vec<ObjectId>, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        if objects.is_empty() || self.state.run.is_some() {
            return effects;
        }
        let id = request_id();
        let progress = Arc::new(Mutex::new(Progress {
            total: objects.len(),
            ..Progress::default()
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        self.state.before = self.state.objects.iter().map(|o| o.id.clone()).collect();
        self.state.result = None;
        self.state.error = None;
        self.state.pending.insert(id, Pending::Recompile);
        self.state.run = Some(Run {
            progress: progress.clone(),
            cancel: cancel.clone(),
            started: Instant::now(),
        });
        let everything = objects.len() == self.compilable().count();
        let utl_recomp = (self.state.utl_recomp && everything).then(|| {
            objects
                .iter()
                .map(|o| o.owner.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        });
        effects.recompile = Some(RecompileRequest {
            id,
            objects,
            parallelism: cx.settings.invalid.parallelism.max(1),
            progress,
            cancel,
            utl_recomp,
        });
        effects
    }

    fn compilable(&self) -> impl Iterator<Item = &InvalidObject> {
        self.state.objects.iter().filter(|o| o.compilable)
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        let DbEvent::TaskDone { id, result } = event else {
            return Effects::default();
        };
        let Some(pending) = self.state.pending.remove(&id) else {
            return Effects::default();
        };
        let s = &mut self.state;
        match (pending, result) {
            (Pending::Load, Ok(any)) => {
                if let Ok(objects) = any.downcast::<Vec<InvalidObject>>() {
                    s.objects = *objects;
                    s.error = None;
                    let ids: BTreeSet<&ObjectId> = s.objects.iter().map(|o| &o.id).collect();
                    s.selected.retain(|id| ids.contains(id));
                    if !s.before.is_empty() {
                        s.result = Some(fixed_text(&s.before, &s.objects, s.result.take()));
                        s.before.clear();
                    }
                }
            }
            (Pending::Load, Err(e)) => s.error = Some(e.to_string()),
            (Pending::Recompile, result) => {
                s.run = None;
                let mut text = String::new();
                match result.map(|any| any.downcast::<RecompileOutcome>()) {
                    Ok(Ok(outcome)) => {
                        let RecompileOutcome { summary, note } = *outcome;
                        text = format!(
                            "Recompiled {} in {:.1} s{}",
                            summary.compiled,
                            summary.elapsed.as_secs_f64(),
                            note.map(|n| format!(" {n}")).unwrap_or_default()
                        );
                        if summary.cancelled {
                            text.push_str(" (cancelled)");
                        }
                        if !summary.failed.is_empty() {
                            text.push_str(&format!(
                                "; {} could not be compiled: {}",
                                summary.failed.len(),
                                summary
                                    .failed
                                    .iter()
                                    .take(3)
                                    .map(|(id, e)| format!("{} ({e})", as_shown(&id.name)))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                        }
                    }
                    Ok(Err(_)) => {}
                    Err(e) => s.error = Some(e.to_string()),
                }
                s.result = Some(text);
                let mut effects = self.refresh();
                effects.invalid_changed = true;
                return effects;
            }
        }
        Effects::default()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        if !cx.connected {
            ui.centered_and_justified(|ui| ui.weak("Connect to see invalid objects"));
            return effects;
        }
        if !self.state.loaded {
            effects.extend(self.refresh());
        }
        if self.recompile_on_load && self.state.loaded && !self.is_busy() {
            // Opened by the browser's button: everything shown.
            self.recompile_on_load = false;
            let all = self.compilable().map(|o| o.id.clone()).collect();
            effects.extend(self.recompile(all, cx));
        }
        let mut refresh = false;
        // Header: what's shown.
        ui.horizontal(|ui| {
            let schemas: BTreeSet<String> = self
                .state
                .objects
                .iter()
                .map(|o| o.id.owner.clone())
                .chain(self.schema.clone())
                .collect();
            let schema_label = self
                .schema
                .clone()
                .unwrap_or_else(|| "All user schemas".into());
            egui::ComboBox::from_id_salt(("invalid-schema", cx.id))
                .selected_text(schema_label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.schema.is_none(), "All user schemas")
                        .clicked()
                    {
                        self.schema = None;
                        refresh = true;
                    }
                    for s in schemas {
                        if ui
                            .selectable_label(self.schema.as_ref() == Some(&s), &s)
                            .clicked()
                        {
                            self.schema = Some(s);
                            refresh = true;
                        }
                    }
                });
            let types: BTreeSet<String> = self
                .state
                .objects
                .iter()
                .map(|o| o.id.object_type.clone())
                .collect();
            egui::ComboBox::from_id_salt(("invalid-type", cx.id))
                .selected_text(
                    self.object_type
                        .as_deref()
                        .map_or("All types".to_owned(), str::to_lowercase),
                )
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.object_type, None, "All types");
                    for t in types {
                        let label = t.to_lowercase();
                        ui.selectable_value(&mut self.object_type, Some(t), label);
                    }
                });
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter by name")
                    .desired_width(180.0),
            );
            if pythia_ui::icons::icon_button(ui, Icon::Refresh, "Refresh").clicked() {
                refresh = true;
            }
            if self.is_busy() && self.state.run.is_none() {
                ui.spinner();
            }
        });
        if refresh {
            effects.extend(self.refresh());
        }

        let filter = self.filter.trim().to_uppercase();
        let shown: Vec<usize> = self
            .state
            .objects
            .iter()
            .enumerate()
            .filter(|(_, o)| {
                self.object_type
                    .as_ref()
                    .is_none_or(|t| &o.id.object_type == t)
                    && crate::browser::name_matches(&o.id.name, &filter)
            })
            .map(|(i, _)| i)
            .collect();

        // Recompiling.
        ui.horizontal(|ui| {
            let running = self.state.run.is_some();
            let selected: Vec<ObjectId> = self
                .compilable()
                .filter(|o| self.state.selected.contains(&o.id))
                .map(|o| o.id.clone())
                .collect();
            let all: Vec<ObjectId> = self.compilable().map(|o| o.id.clone()).collect();
            if ui
                .add_enabled(
                    !running && !selected.is_empty(),
                    egui::Button::new(format!("Recompile selected ({})", selected.len())),
                )
                .clicked()
            {
                effects.extend(self.recompile(selected, cx));
            }
            if ui
                .add_enabled(
                    !running && !all.is_empty(),
                    egui::Button::new(format!("Recompile all ({})", all.len())),
                )
                .clicked()
            {
                effects.extend(self.recompile(all, cx));
            }
            let sysdba = cx.privilege == Some(pythia_config::Privilege::Sysdba);
            if sysdba {
                ui.checkbox(&mut self.state.utl_recomp, "Use UTL_RECOMP")
                    .on_hover_text(
                        "Recompile all with UTL_RECOMP.RECOMP_PARALLEL (as utlrp.sql), \
                         through scheduler jobs; falls back to Pythia's own way",
                    );
            }
            ui.weak(format!(
                "{} session(s), see Settings → Invalid objects",
                cx.settings.invalid.parallelism.max(1)
            ));
        });
        if let Some(run) = &self.state.run {
            let p = run.progress.lock().expect("progress").clone();
            ui.horizontal(|ui| {
                let fraction = if p.total == 0 {
                    0.0
                } else {
                    p.done as f32 / p.total as f32
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(240.0)
                        .text(format!("{} of {}", p.done, p.total)),
                );
                if p.waves > 0 {
                    ui.label(format!("wave {} of {}", p.wave.max(1), p.waves));
                }
                ui.weak(format!("{:.1} s", run.started.elapsed().as_secs_f64()));
                let cancelling = run.cancel.load(Ordering::Relaxed);
                if ui
                    .add_enabled(!cancelling, egui::Button::new("Cancel"))
                    .on_hover_text("Stop after the objects being compiled now")
                    .clicked()
                {
                    run.cancel.store(true, Ordering::Relaxed);
                }
            });
            if let Some(current) = p.running.first() {
                ui.weak(format!("Compiling {current}"));
            }
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        if let Some(result) = &self.state.result {
            ui.label(result);
        }
        if let Some(e) = &self.state.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.add_space(4.0);

        let mut open = None;
        let mut toggle = None;
        let row_height = pythia_ui::row_height(ui);
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(TableColumn::exact(24.0))
            .column(TableColumn::initial(110.0))
            .column(TableColumn::initial(110.0))
            .column(TableColumn::initial(200.0))
            .column(TableColumn::initial(170.0))
            .column(TableColumn::initial(50.0))
            .column(TableColumn::remainder())
            .header(row_height, |mut h| {
                for title in [
                    "",
                    "Owner",
                    "Type",
                    "Name",
                    "Last DDL",
                    "Errors",
                    "First error",
                ] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, shown.len(), |mut row| {
                    let o = &self.state.objects[shown[row.index()]];
                    row.col(|ui| {
                        if o.compilable {
                            let mut on = self.state.selected.contains(&o.id);
                            if ui.checkbox(&mut on, "").changed() {
                                toggle = Some(o.id.clone());
                            }
                        } else {
                            ui.add(Icon::Lock.sized(ui, pythia_ui::icons::IconSize::Small))
                                .on_hover_text(
                                    "You may not compile this object (no ALTER ANY … privilege)",
                                );
                        }
                    });
                    let text = |ui: &mut Ui, t: &str| {
                        let mut t = RichText::new(t).monospace();
                        if !o.compilable {
                            t = t.weak();
                        }
                        ui.add(egui::Label::new(t).truncate().sense(egui::Sense::click()))
                    };
                    row.col(|ui| {
                        text(ui, &as_shown(&o.id.owner));
                    });
                    row.col(|ui| {
                        text(ui, &o.id.object_type.to_lowercase());
                    });
                    row.col(|ui| {
                        let r = text(ui, &as_shown(&o.id.name))
                            .on_hover_text("Double-click to open it at its first error");
                        if r.double_clicked() {
                            open = Some(o.clone());
                        }
                    });
                    row.col(|ui| {
                        text(ui, &o.last_ddl);
                    });
                    row.col(|ui| {
                        text(ui, &o.errors.to_string());
                    });
                    row.col(|ui| {
                        if let Some((line, error)) = &o.first_error {
                            let r = ui
                                .add(
                                    egui::Label::new(
                                        RichText::new(format!("{line}: {error}"))
                                            .color(ui.visuals().error_fg_color),
                                    )
                                    .truncate()
                                    .sense(egui::Sense::click()),
                                )
                                .on_hover_text(error);
                            if r.double_clicked() {
                                open = Some(o.clone());
                            }
                        }
                    });
                });
            });
        if shown.is_empty() && self.state.loaded && !self.is_busy() {
            ui.weak("No invalid objects");
        }
        if let Some(id) = toggle
            && !self.state.selected.remove(&id)
        {
            self.state.selected.insert(id);
        }
        if let (Some(o), Some(connection)) = (open, cx.connection.clone()) {
            effects.open.push(open_request(&o, connection));
        }
        effects
    }
}

/// A name as the object browser shows it (#272).
fn as_shown(name: &str) -> String {
    crate::browser::shown_name(Dialect::ORACLE, name)
}

/// Opens an invalid object: its source at the first error, or its view.
fn open_request(o: &InvalidObject, connection: String) -> OpenRequest {
    let object_type = ObjectType::from_dictionary(&o.id.object_type);
    let object = |object_type| ObjectRef {
        owner: o.id.owner.clone(),
        name: o.id.name.clone(),
        object_type,
    };
    match object_type {
        Some(t) if t.has_source() => match &o.first_error {
            Some((line, _)) => OpenRequest::ProgramAt {
                connection,
                object: object(t),
                line: *line as usize,
            },
            None => OpenRequest::Program {
                connection,
                object: object(t),
            },
        },
        Some(t) => OpenRequest::Object {
            connection,
            object: object(t),
        },
        None => OpenRequest::Object {
            connection,
            object: object(ObjectType::Table),
        },
    }
}

/// "N valid now, M still invalid", after the recompile's own text.
fn fixed_text(before: &BTreeSet<ObjectId>, now: &[InvalidObject], text: Option<String>) -> String {
    let still = now.iter().filter(|o| before.contains(&o.id)).count();
    let fixed = before.len().saturating_sub(still);
    let counts = format!("{fixed} valid now, {still} still invalid");
    match text.filter(|t| !t.is_empty()) {
        Some(t) => format!("{t}: {counts}"),
        None => counts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_text_counts_what_was_fixed() {
        let id = |n: &str| ObjectId::new("HR", n, "VIEW");
        let before: BTreeSet<ObjectId> = [id("A"), id("B"), id("C")].into();
        let now = vec![InvalidObject {
            id: id("B"),
            last_ddl: String::new(),
            errors: 1,
            first_error: None,
            compilable: true,
        }];
        assert_eq!(
            fixed_text(&before, &now, Some("Recompiled 3 in 0.1 s".into())),
            "Recompiled 3 in 0.1 s: 2 valid now, 1 still invalid"
        );
    }
}
