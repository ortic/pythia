//! egui views for schema and data compare.
//!
//! The views never touch a connection. They return [`CompareRequest`]s
//! (closures to run as `DbRequest::Task` on a named connection) and receive
//! the results through `on_task_done`. The host app decides which session
//! runs them; a dedicated session per connection keeps SQL windows free.
//!
//! Design: a clean summary first (counts by status, clickable as filters),
//! then the objects grouped by status with a side-by-side diff that
//! highlights changed words, and a sync script that is safe by default
//! (destructive statements commented out until explicitly enabled).

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, FontId, Layout, RichText, Sense, Ui, vec2};
use pythia_db::{Cell, Column, DbError, Dialect, ObjectType, TaskFn, TaskResult};
use pythia_ui::code_editor::{CodeEditor, EditorConfig};
use pythia_ui::icons::{Icon, icon_text_button};
use pythia_ui::{GridState, ResultGrid};

use crate::data::{self, Chunk, DataDiff, DataOptions, DataSide, DataSource, Merger};
use crate::diff::{self, LineDiff, RowKind};
use crate::schema::{
    self, ObjectResult, Options, SchemaDiff, SchemaSnapshot, Status, TypeSnapshot,
};

/// A task to run on `connection` (as `DbRequest::Task { id, task }`).
pub struct CompareRequest {
    pub id: u64,
    pub connection: String,
    pub task: TaskFn,
}

/// Something the host should do on the user's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareAction {
    /// Open `text` in a new SQL window on `connection` (not executed).
    OpenSql { connection: String, text: String },
}

#[derive(Default)]
pub struct CompareOutput {
    pub requests: Vec<CompareRequest>,
    pub actions: Vec<CompareAction>,
}

/// Request ids live in their own range so they never collide with the
/// host's ids (which count up from 1).
static NEXT_ID: AtomicU64 = AtomicU64::new(1 << 56);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn task<T: Any + Send>(
    f: impl FnOnce(&pythia_db::oracle::Connection) -> Result<T, crate::Error> + Send + 'static,
) -> TaskFn {
    Box::new(move |conn| {
        f(conn)
            .map(|v| Box::new(v) as Box<dyn Any + Send>)
            .map_err(DbError::from)
    })
}

fn take<T: Any>(result: TaskResult) -> Result<T, String> {
    match result {
        Ok(value) => value
            .downcast::<T>()
            .map(|b| *b)
            .map_err(|_| "unexpected task result".to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

fn load_schemas(conn: &pythia_db::oracle::Connection) -> Result<Vec<String>, crate::Error> {
    let release = pythia_db::compat::release(conn)?;
    let sql = format!(
        "select username from all_users where username in ({}) or username = user order by username",
        pythia_db::compat::schemas_query(release, false)
    );
    let rows = conn.query_as::<String>(&sql, &[])?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A connection + schema pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub connection: Option<String>,
    pub schema: String,
}

/// Schema lists per connection, shared by the endpoint pickers.
#[derive(Default)]
struct SchemaLists {
    lists: HashMap<String, Result<Vec<String>, String>>,
    pending: HashMap<u64, String>,
}

impl SchemaLists {
    fn ensure(&mut self, connection: &str, out: &mut CompareOutput) {
        if self.lists.contains_key(connection) || self.pending.values().any(|c| c == connection) {
            return;
        }
        let id = next_id();
        self.pending.insert(id, connection.to_owned());
        out.requests.push(CompareRequest {
            id,
            connection: connection.to_owned(),
            task: task(load_schemas),
        });
    }

    fn on_task_done(&mut self, id: u64, result: TaskResult) -> Option<TaskResult> {
        match self.pending.remove(&id) {
            Some(connection) => {
                self.lists.insert(connection, take::<Vec<String>>(result));
                None
            }
            None => Some(result),
        }
    }
}

/// Connection and schema pickers; returns true if something changed.
fn endpoint_ui(
    ui: &mut Ui,
    id: &str,
    label: &str,
    endpoint: &mut Endpoint,
    connections: &[String],
    lists: &mut SchemaLists,
    out: &mut CompareOutput,
) -> bool {
    let before = endpoint.clone();
    ui.vertical(|ui| {
        ui.label(RichText::new(label).small().weak());
        ui.horizontal(|ui| {
            let current = endpoint
                .connection
                .clone()
                .unwrap_or_else(|| "Choose connection".into());
            egui::ComboBox::from_id_salt((id, "conn"))
                .selected_text(current)
                .width(180.0)
                .show_ui(ui, |ui| {
                    for c in connections {
                        if ui
                            .selectable_label(endpoint.connection.as_deref() == Some(c.as_str()), c)
                            .clicked()
                        {
                            endpoint.connection = Some(c.clone());
                        }
                    }
                    if connections.is_empty() {
                        ui.weak("No open connections");
                    }
                });
            if let Some(conn) = endpoint.connection.clone() {
                lists.ensure(&conn, out);
                let schema_text = if endpoint.schema.is_empty() {
                    "Schema".to_owned()
                } else {
                    endpoint.schema.clone()
                };
                egui::ComboBox::from_id_salt((id, "schema"))
                    .selected_text(schema_text)
                    .width(160.0)
                    .height(400.0)
                    .show_ui(ui, |ui| match lists.lists.get(&conn) {
                        Some(Ok(schemas)) => {
                            for s in schemas {
                                ui.selectable_value(&mut endpoint.schema, s.clone(), s);
                            }
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                        None => {
                            ui.spinner();
                        }
                    });
            }
        });
    });
    *endpoint != before
}

fn status_color(ui: &Ui, status: Status) -> Color32 {
    let dark = ui.visuals().dark_mode;
    match status {
        Status::Different => {
            if dark {
                Color32::from_rgb(0xe8, 0xb3, 0x4a)
            } else {
                Color32::from_rgb(0xa8, 0x62, 0x00)
            }
        }
        Status::OnlyInSource => {
            if dark {
                Color32::from_rgb(0x5c, 0xc9, 0x7b)
            } else {
                Color32::from_rgb(0x1a, 0x7f, 0x37)
            }
        }
        Status::OnlyInTarget => {
            if dark {
                Color32::from_rgb(0xf2, 0x6d, 0x6d)
            } else {
                Color32::from_rgb(0xc8, 0x2b, 0x2b)
            }
        }
        Status::Equal => ui.visuals().weak_text_color(),
    }
}

fn status_icon(status: Status) -> Icon {
    match status {
        Status::Different => Icon::Warning,
        Status::OnlyInSource => Icon::Plus,
        Status::OnlyInTarget => Icon::Minus,
        Status::Equal => Icon::Success,
    }
}

/// A summary card: big number, label, clickable as a filter.
fn summary_card(
    ui: &mut Ui,
    value: String,
    label: &str,
    color: Color32,
    selected: bool,
) -> egui::Response {
    let fill = if selected {
        color.gamma_multiply(0.18)
    } else {
        ui.visuals().faint_bg_color
    };
    let stroke = if selected {
        egui::Stroke::new(1.5, color)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    let inner = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(14, 8))
        .show(ui, |ui| {
            ui.set_min_width(118.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(value).size(22.0).strong().color(color));
                ui.label(RichText::new(label).small());
            });
        });
    inner
        .response
        .interact(Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Colors for diff rows and changed words.
struct DiffColors {
    removed: Color32,
    added: Color32,
    removed_strong: Color32,
    added_strong: Color32,
    gutter: Color32,
}

impl DiffColors {
    fn new(ui: &Ui) -> Self {
        let dark = ui.visuals().dark_mode;
        let (red, green) = if dark {
            (
                Color32::from_rgb(0xf8, 0x51, 0x49),
                Color32::from_rgb(0x3f, 0xb9, 0x50),
            )
        } else {
            (
                Color32::from_rgb(0xd1, 0x24, 0x2f),
                Color32::from_rgb(0x1a, 0x7f, 0x37),
            )
        };
        let (weak, strong) = if dark { (0.14, 0.38) } else { (0.10, 0.28) };
        Self {
            removed: red.gamma_multiply(weak),
            added: green.gamma_multiply(weak),
            removed_strong: red.gamma_multiply(strong),
            added_strong: green.gamma_multiply(strong),
            gutter: ui.visuals().weak_text_color(),
        }
    }
}

/// Side-by-side diff with one shared scroll area (scrolling is
/// synchronized by construction) and word-level highlights.
pub fn side_by_side_ui(ui: &mut Ui, id: impl egui::AsIdSalt, d: &LineDiff, jump_to: Option<usize>) {
    let colors = DiffColors::new(ui);
    let font = FontId::monospace(12.5);
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 3.0;
    let text_color = ui.visuals().text_color();
    let digits = d
        .rows
        .iter()
        .filter_map(|r| {
            r.left
                .as_ref()
                .map(|s| s.line)
                .max(r.right.as_ref().map(|s| s.line))
        })
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let gutter_w = ui.fonts_mut(|f| f.glyph_width(&font, '0')) * (digits as f32 + 1.5);
    let mut area = egui::ScrollArea::vertical().id_salt(id).auto_shrink(false);
    if let Some(row) = jump_to {
        area = area.vertical_scroll_offset((row as f32 * row_h - 3.0 * row_h).max(0.0));
    }
    area.show_rows(ui, row_h, d.rows.len(), |ui, range| {
        let width = ui.available_width();
        let half = (width - 9.0) / 2.0;
        for row in &d.rows[range] {
            let (rect, response) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
            let painter = ui.painter_at(rect);
            let left_rect = egui::Rect::from_min_size(rect.min, vec2(half, row_h));
            let right_rect =
                egui::Rect::from_min_size(rect.min + vec2(half + 9.0, 0.0), vec2(half, row_h));
            let (lbg, rbg) = match row.kind {
                RowKind::Equal => (Color32::TRANSPARENT, Color32::TRANSPARENT),
                RowKind::Changed => (colors.removed, colors.added),
                RowKind::Removed => (colors.removed, ui.visuals().faint_bg_color),
                RowKind::Added => (ui.visuals().faint_bg_color, colors.added),
            };
            painter.rect_filled(left_rect, 0.0, lbg);
            painter.rect_filled(right_rect, 0.0, rbg);
            for (side, r, strong) in [
                (&row.left, left_rect, colors.removed_strong),
                (&row.right, right_rect, colors.added_strong),
            ] {
                let Some(side) = side else { continue };
                painter.text(
                    r.min + vec2(gutter_w - 6.0, row_h / 2.0),
                    egui::Align2::RIGHT_CENTER,
                    side.line.to_string(),
                    font.clone(),
                    colors.gutter,
                );
                let mut job = LayoutJob::default();
                let mut pos = 0;
                for span in &side.changes {
                    if span.start > pos {
                        job.append(
                            &side.text[pos..span.start],
                            0.0,
                            TextFormat::simple(font.clone(), text_color),
                        );
                    }
                    let format = TextFormat {
                        font_id: font.clone(),
                        color: text_color,
                        background: strong,
                        ..Default::default()
                    };
                    job.append(&side.text[span.clone()], 0.0, format);
                    pos = span.end;
                }
                job.append(
                    &side.text[pos.min(side.text.len())..],
                    0.0,
                    TextFormat::simple(font.clone(), text_color),
                );
                let galley = ui.fonts_mut(|f| f.layout_job(job));
                let text_pos = r.min + vec2(gutter_w, (row_h - galley.size().y) / 2.0);
                ui.painter_at(r).galley(text_pos, galley, text_color);
            }
            let long = row.left.as_ref().is_some_and(|s| s.text.len() > 80)
                || row.right.as_ref().is_some_and(|s| s.text.len() > 80);
            if long && response.hovered() {
                response.on_hover_ui(|ui| {
                    if let Some(s) = &row.left {
                        ui.label(RichText::new(format!("− {}", s.text)).monospace());
                    }
                    if let Some(s) = &row.right {
                        ui.label(RichText::new(format!("+ {}", s.text)).monospace());
                    }
                });
            }
        }
    });
}

/// A read-only script with Copy / Open in SQL window.
struct ScriptView {
    editor: CodeEditor,
    text: String,
}

impl ScriptView {
    fn new() -> Self {
        Self {
            editor: CodeEditor::new(""),
            text: String::new(),
        }
    }

    fn set(&mut self, text: String) {
        if text != self.text {
            self.editor.set_text(&text);
            self.text = text;
        }
    }

    fn ui(
        &mut self,
        ui: &mut Ui,
        id: impl egui::AsId,
        connection: Option<&str>,
        out: &mut CompareOutput,
    ) {
        ui.horizontal(|ui| {
            let disabled = self
                .text
                .lines()
                .filter(|l| l.starts_with("-- ") && !l.starts_with("-- WARNING") && l.contains(';'))
                .count();
            let warnings = self
                .text
                .lines()
                .filter(|l| l.starts_with("-- WARNING"))
                .count();
            if warnings > 0 {
                ui.add(Icon::Warning.image(ui));
                ui.label(format!("{warnings} warning(s)"));
            }
            if disabled > 0 {
                ui.weak(format!(
                    "· {disabled} statement(s) commented out for safety"
                ));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(conn) = connection
                    && icon_text_button(ui, Icon::NewSql, "Open in SQL window")
                        .on_hover_text(format!("Opens the script on {conn} without running it"))
                        .clicked()
                {
                    out.actions.push(CompareAction::OpenSql {
                        connection: conn.to_owned(),
                        text: self.text.clone(),
                    });
                }
                if icon_text_button(ui, Icon::Copy, "Copy").clicked() {
                    ui.ctx().copy_text(self.text.clone());
                }
            });
        });
        ui.separator();
        self.editor.set_id_salt(id);
        self.editor.ui(
            ui,
            &EditorConfig {
                read_only: true,
                ..EditorConfig::default()
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Schema compare
// ---------------------------------------------------------------------------

enum SchemaState {
    Setup,
    Running {
        pending: HashMap<u64, (DataSide, ObjectType)>,
        total: usize,
        source: SchemaSnapshot,
        target: SchemaSnapshot,
        failed: Vec<String>,
    },
    Done(SchemaDiff),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaTab {
    Objects,
    Script,
}

/// Schema compare document.
pub struct SchemaCompare {
    pub source: Endpoint,
    pub target: Endpoint,
    pub options: Options,
    lists: SchemaLists,
    state: SchemaState,
    error: Option<String>,
    /// Status filter from the summary cards (None = everything but Equal).
    filter_status: Option<Status>,
    filter_text: String,
    selected: Option<usize>,
    selected_diff: Option<(usize, LineDiff)>,
    jump: Option<usize>,
    unified: bool,
    tab: SchemaTab,
    script: ScriptView,
}

impl Default for SchemaCompare {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaCompare {
    pub fn new() -> Self {
        Self {
            source: Endpoint::default(),
            target: Endpoint::default(),
            options: Options::default(),
            lists: SchemaLists::default(),
            state: SchemaState::Setup,
            error: None,
            filter_status: None,
            filter_text: String::new(),
            selected: None,
            selected_diff: None,
            jump: None,
            unified: false,
            tab: SchemaTab::Objects,
            script: ScriptView::new(),
        }
    }

    pub fn title(&self) -> String {
        match (self.source.schema.as_str(), self.target.schema.as_str()) {
            ("", _) | (_, "") => "Schema compare".into(),
            (s, t) => format!("{} → {}", s.to_lowercase(), t.to_lowercase()),
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.state, SchemaState::Running { .. })
    }

    pub fn result(&self) -> Option<&SchemaDiff> {
        match &self.state {
            SchemaState::Done(d) => Some(d),
            _ => None,
        }
    }

    /// Feeds a finished task back. Returns false if the id isn't ours.
    pub fn on_task_done(&mut self, id: u64, result: TaskResult) -> bool {
        let Some(result) = self.lists.on_task_done(id, result) else {
            return true;
        };
        let SchemaState::Running {
            pending,
            source,
            target,
            failed,
            ..
        } = &mut self.state
        else {
            return false;
        };
        let Some((side, object_type)) = pending.remove(&id) else {
            return false;
        };
        match take::<TypeSnapshot>(result) {
            Ok(part) => match side {
                DataSide::Source => source.add(part),
                DataSide::Target => target.add(part),
            },
            Err(e) => failed.push(format!("{} ({:?}): {e}", object_type.plural(), side)),
        }
        if pending.is_empty() {
            let SchemaState::Running {
                source,
                target,
                failed,
                ..
            } = std::mem::replace(&mut self.state, SchemaState::Setup)
            else {
                unreachable!()
            };
            let mut diff = schema::compare(&source, &target, &self.options);
            diff.errors.extend(failed);
            self.selected = diff.objects.iter().position(|o| o.status != Status::Equal);
            self.selected_diff = None;
            self.state = SchemaState::Done(diff);
            self.refresh_script();
        }
        true
    }

    /// Starts (or restarts) the compare with the current endpoints and options.
    pub fn compare(&mut self) -> CompareOutput {
        let mut out = CompareOutput::default();
        self.start(&mut out);
        out
    }

    /// Selects an object in the result list by name.
    pub fn select_object(&mut self, name: &str) {
        if let SchemaState::Done(d) = &self.state {
            self.selected = d
                .objects
                .iter()
                .position(|o| o.key.name.eq_ignore_ascii_case(name));
            self.selected_diff = None;
            self.tab = SchemaTab::Objects;
        }
    }

    /// Switches between the object list and the full sync script.
    pub fn show_script(&mut self, script: bool) {
        self.tab = if script {
            SchemaTab::Script
        } else {
            SchemaTab::Objects
        };
    }

    fn start(&mut self, out: &mut CompareOutput) {
        let (Some(sc), Some(tc)) = (
            self.source.connection.clone(),
            self.target.connection.clone(),
        ) else {
            return;
        };
        let mut pending = HashMap::new();
        for (side, conn, schema) in [
            (DataSide::Source, sc, self.source.schema.clone()),
            (DataSide::Target, tc, self.target.schema.clone()),
        ] {
            for t in &self.options.types {
                let id = next_id();
                pending.insert(id, (side, *t));
                let (schema, t, options) = (schema.clone(), *t, self.options.clone());
                out.requests.push(CompareRequest {
                    id,
                    connection: conn.clone(),
                    task: task(move |c| schema::collect_type(c, &schema, t, &options)),
                });
            }
        }
        let total = pending.len();
        self.error = None;
        self.state = SchemaState::Running {
            pending,
            total,
            source: SchemaSnapshot::new(self.source.schema.clone()),
            target: SchemaSnapshot::new(self.target.schema.clone()),
            failed: Vec::new(),
        };
    }

    fn refresh_script(&mut self) {
        if let SchemaState::Done(d) = &self.state {
            let title = format!(
                "Sync {} → {} (generated by Pythia)\nApplies source definitions to the target schema.",
                d.source_schema, d.target_schema
            );
            self.script.set(d.script(&title));
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, connections: &[String]) -> CompareOutput {
        let mut out = CompareOutput::default();
        self.setup_ui(ui, connections, &mut out);
        ui.separator();
        match &self.state {
            SchemaState::Setup => {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.add(Icon::Database.sized(ui, pythia_ui::icons::IconSize::Large));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Compare two schemas").heading());
                    ui.weak("Pick a source and a target, then Compare. The sync script turns the target into the source.");
                    if let Some(e) = &self.error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                });
            }
            SchemaState::Running { pending, total, .. } => {
                let done = total - pending.len();
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "Reading object definitions… {done} of {total} steps"
                    ));
                });
                ui.add(
                    egui::ProgressBar::new(done as f32 / (*total).max(1) as f32)
                        .desired_width(320.0),
                );
            }
            SchemaState::Done(_) => self.results_ui(ui, &mut out),
        }
        out
    }

    fn setup_ui(&mut self, ui: &mut Ui, connections: &[String], out: &mut CompareOutput) {
        let busy = self.is_busy();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                endpoint_ui(
                    ui,
                    "src",
                    "SOURCE (reference)",
                    &mut self.source,
                    connections,
                    &mut self.lists,
                    out,
                );
                ui.add_space(4.0);
                ui.add(Icon::ChevronRight.mono(ui.visuals().weak_text_color()));
                ui.add_space(4.0);
                endpoint_ui(
                    ui,
                    "tgt",
                    "TARGET (to update)",
                    &mut self.target,
                    connections,
                    &mut self.lists,
                    out,
                );
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(" ").small());
                    ui.horizontal(|ui| {
                        ui.menu_button("Options", |ui| self.options_ui(ui));
                        let ready = self.source.connection.is_some()
                            && self.target.connection.is_some()
                            && !self.source.schema.is_empty()
                            && !self.target.schema.is_empty()
                            && !self.options.types.is_empty();
                        let same = self.source == self.target;
                        let button = ui
                            .add_enabled(
                                ready && !same,
                                egui::Button::image_and_text(
                                    Icon::Execute.image(ui),
                                    RichText::new("Compare").strong(),
                                ),
                            )
                            .on_disabled_hover_text(if same {
                                "Source and target are the same"
                            } else {
                                "Choose both schemas"
                            });
                        if button.clicked() {
                            self.start(out);
                        }
                    });
                });
            });
        });
    }

    fn options_ui(&mut self, ui: &mut Ui) {
        ui.set_min_width(260.0);
        ui.label(RichText::new("Object types").strong());
        ui.horizontal_wrapped(|ui| {
            for t in schema::TYPES {
                let mut on = self.options.types.contains(t);
                if ui.checkbox(&mut on, t.plural()).changed() {
                    if on {
                        self.options.types.push(*t);
                    } else {
                        self.options.types.retain(|x| x != t);
                    }
                }
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Name filter");
            ui.add(
                egui::TextEdit::singleline(&mut self.options.name_filter)
                    .hint_text("e.g. EMP*")
                    .desired_width(140.0),
            );
        });
        ui.separator();
        ui.label(RichText::new("Treat as equal when only").strong());
        ui.checkbox(
            &mut self.options.normalize.ignore_whitespace,
            "whitespace differs",
        );
        ui.checkbox(
            &mut self.options.normalize.ignore_case,
            "letter case of keywords/names differs",
        );
        ui.checkbox(
            &mut self.options.normalize.ignore_comments,
            "comments differ",
        );
        ui.weak("Storage, tablespaces and system generated constraint names are always ignored.");
        ui.separator();
        ui.checkbox(&mut self.options.include_drops, "Enable DROP statements in the script")
            .on_hover_text("Off: objects and columns that only exist in the target are listed as commented-out drops.");
    }

    fn results_ui(&mut self, ui: &mut Ui, out: &mut CompareOutput) {
        let SchemaState::Done(diff) = &self.state else {
            return;
        };
        // Summary.
        let mut clicked_status = None;
        let mut restart = false;
        ui.horizontal(|ui| {
            for status in [
                Status::Different,
                Status::OnlyInSource,
                Status::OnlyInTarget,
                Status::Equal,
            ] {
                let color = status_color(ui, status);
                if summary_card(
                    ui,
                    diff.count(status).to_string(),
                    status.label(),
                    color,
                    self.filter_status == Some(status),
                )
                .clicked()
                {
                    clicked_status = Some(status);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_text_button(ui, Icon::Refresh, "Compare again").clicked() {
                    restart = true;
                }
            });
        });
        if let Some(s) = clicked_status {
            self.filter_status = if self.filter_status == Some(s) {
                None
            } else {
                Some(s)
            };
        }
        if restart {
            self.start(out);
            return;
        }
        let SchemaState::Done(diff) = &self.state else {
            return;
        };
        if diff.is_equal() {
            ui.horizontal(|ui| {
                ui.add(Icon::Success.image(ui));
                ui.label(
                    RichText::new("The schemas are identical for the selected object types.")
                        .strong(),
                );
            });
        }
        for e in &diff.errors {
            ui.horizontal(|ui| {
                ui.add(Icon::Error.image(ui));
                ui.colored_label(ui.visuals().error_fg_color, e);
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, SchemaTab::Objects, "Objects");
            let included = diff
                .objects
                .iter()
                .filter(|o| o.included && o.status != Status::Equal)
                .count();
            ui.selectable_value(
                &mut self.tab,
                SchemaTab::Script,
                format!("Sync script · {included} objects"),
            );
        });
        ui.separator();
        match self.tab {
            SchemaTab::Objects => self.objects_ui(ui),
            SchemaTab::Script => {
                let conn = self.target.connection.clone();
                self.script
                    .ui(ui, "schema-sync-script", conn.as_deref(), out);
            }
        }
    }

    fn objects_ui(&mut self, ui: &mut Ui) {
        let mut include_changed = false;
        let mut select = None;
        egui::Panel::left(egui::Id::new("schema-compare-list"))
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter_text)
                        .hint_text("Filter objects…")
                        .desired_width(f32::INFINITY),
                );
                let SchemaState::Done(diff) = &mut self.state else {
                    return;
                };
                let filter = self.filter_text.to_uppercase();
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for status in [
                            Status::Different,
                            Status::OnlyInSource,
                            Status::OnlyInTarget,
                            Status::Equal,
                        ] {
                            let visible = match self.filter_status {
                                Some(f) => f == status,
                                None => status != Status::Equal,
                            };
                            let indices: Vec<usize> = diff
                                .objects
                                .iter()
                                .enumerate()
                                .filter(|(_, o)| {
                                    o.status == status && name_matches(&o.key.name, &filter)
                                })
                                .map(|(i, _)| i)
                                .collect();
                            if !visible || indices.is_empty() {
                                continue;
                            }
                            let color = status_color(ui, status);
                            egui::CollapsingHeader::new(
                                RichText::new(format!("{}  {}", status.label(), indices.len()))
                                    .strong()
                                    .color(color),
                            )
                            .id_salt(("status", status as u8))
                            .default_open(true)
                            .show(ui, |ui| {
                                for i in indices {
                                    let o = &mut diff.objects[i];
                                    ui.horizontal(|ui| {
                                        if o.status != Status::Equal
                                            && ui
                                                .checkbox(&mut o.included, "")
                                                .on_hover_text("Include in the sync script")
                                                .changed()
                                        {
                                            include_changed = true;
                                        }
                                        if let Some(icon) = Icon::for_object_type(
                                            o.key.object_type.dictionary_name(),
                                        ) {
                                            ui.add(
                                                icon.sized(ui, pythia_ui::icons::IconSize::Small),
                                            );
                                        }
                                        let label =
                                            RichText::new(o.key.name.to_lowercase()).monospace();
                                        let r =
                                            ui.selectable_label(self.selected == Some(i), label);
                                        let r = r.on_hover_text(format!(
                                            "{} · {}",
                                            o.key.object_type.dictionary_name().to_lowercase(),
                                            o.status.label()
                                        ));
                                        if r.clicked() {
                                            select = Some(i);
                                        }
                                        if o.statements.iter().any(|s| s.warning.is_some()) {
                                            ui.add(
                                                Icon::Warning
                                                    .sized(ui, pythia_ui::icons::IconSize::Small),
                                            )
                                            .on_hover_text("Sync has warnings");
                                        }
                                    });
                                }
                            });
                        }
                    });
            });
        if include_changed {
            self.refresh_script();
        }
        if let Some(i) = select {
            self.selected = Some(i);
            self.selected_diff = None;
            self.jump = None;
        }
        egui::CentralPanel::default().show(ui, |ui| self.detail_ui(ui));
    }

    fn detail_ui(&mut self, ui: &mut Ui) {
        let SchemaState::Done(diff) = &self.state else {
            return;
        };
        let Some(i) = self.selected.filter(|i| *i < diff.objects.len()) else {
            ui.weak("Select an object to see its differences.");
            return;
        };
        let o: &ObjectResult = &diff.objects[i];
        if self.selected_diff.as_ref().is_none_or(|(j, _)| *j != i) {
            // Read as before → after: the target now, and as the sync leaves it.
            let d = diff::side_by_side(
                o.target_ddl.as_deref().unwrap_or(""),
                o.source_ddl.as_deref().unwrap_or(""),
            );
            self.selected_diff = Some((i, d));
        }
        let Some((_, line_diff)) = &self.selected_diff else {
            return;
        };
        ui.horizontal(|ui| {
            if let Some(icon) = Icon::for_object_type(o.key.object_type.dictionary_name()) {
                ui.add(icon.image(ui));
            }
            ui.label(
                RichText::new(o.key.name.to_lowercase())
                    .strong()
                    .monospace(),
            );
            ui.weak(o.key.object_type.dictionary_name().to_lowercase());
            let color = status_color(ui, o.status);
            ui.add(status_icon(o.status).mono(color));
            ui.label(RichText::new(o.status.label()).color(color));
            if !line_diff.is_equal() {
                ui.weak(format!(
                    "· sync changes {}, removes {}, adds {} lines",
                    line_diff.changed, line_diff.removed, line_diff.added
                ));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.checkbox(&mut self.unified, "Unified");
                let hunks = line_diff.hunk_starts();
                if !hunks.is_empty()
                    && icon_text_button(ui, Icon::ChevronDown, "Next change").clicked()
                {
                    let next = hunks
                        .iter()
                        .find(|h| Some(**h) > self.jump)
                        .or(hunks.first())
                        .copied();
                    self.jump = next;
                }
            });
        });
        for note in &o.notes {
            ui.horizontal(|ui| {
                ui.add(Icon::Info.sized(ui, pythia_ui::icons::IconSize::Small));
                ui.weak(note);
            });
        }
        // Statements for this object, compact.
        if !o.statements.is_empty() {
            egui::CollapsingHeader::new(format!("Sync statements ({})", o.statements.len()))
                .id_salt(("stmts", i))
                .default_open(
                    o.status == Status::Different && o.key.object_type == ObjectType::Table,
                )
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt(("stmts-scroll", i))
                        .max_height(170.0)
                        .show(ui, |ui| {
                            for s in &o.statements {
                                if let Some(w) = &s.warning {
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            Icon::Warning
                                                .sized(ui, pythia_ui::icons::IconSize::Small),
                                        );
                                        ui.label(
                                            RichText::new(w)
                                                .small()
                                                .color(ui.visuals().warn_fg_color),
                                        );
                                    });
                                }
                                let text = RichText::new(first_lines(&s.sql, 3)).monospace();
                                if s.enabled {
                                    ui.label(text)
                                } else {
                                    ui.label(text.strikethrough().weak()).on_hover_text(
                                        "Disabled for safety (enable drops in Options)",
                                    )
                                };
                            }
                        });
                });
        }
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 9.0) / 2.0;
            ui.add_sized(
                [w, 16.0],
                egui::Label::new(
                    RichText::new(format!("{} · now", diff.target_schema))
                        .small()
                        .weak(),
                ),
            );
            ui.add_space(9.0);
            ui.add_sized(
                [w, 16.0],
                egui::Label::new(
                    RichText::new(format!(
                        "{} · after sync (as {})",
                        diff.target_schema, diff.source_schema
                    ))
                    .small()
                    .weak(),
                ),
            );
        });
        if self.unified {
            let text = diff::unified(
                o.target_ddl.as_deref().unwrap_or(""),
                o.source_ddl.as_deref().unwrap_or(""),
                &format!("{} (now)", diff.target_schema),
                &format!("{} (after sync)", diff.target_schema),
            );
            egui::ScrollArea::both()
                .id_salt(("unified", i))
                .auto_shrink(false)
                .show(ui, |ui| {
                    for line in text.lines() {
                        let color = match line.chars().next() {
                            Some('+') => status_color(ui, Status::OnlyInSource),
                            Some('-') => status_color(ui, Status::OnlyInTarget),
                            Some('@') => ui.visuals().weak_text_color(),
                            _ => ui.visuals().text_color(),
                        };
                        ui.label(RichText::new(line).monospace().color(color));
                    }
                });
        } else {
            let jump = self.jump.take();
            side_by_side_ui(ui, ("sbs", i), line_diff, jump);
        }
    }
}

fn first_lines(text: &str, n: usize) -> String {
    let mut lines: Vec<&str> = text.lines().take(n + 1).collect();
    if lines.len() > n {
        lines.truncate(n);
        lines.push("…");
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Data compare
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataTab {
    Changed,
    OnlyInSource,
    OnlyInTarget,
    Script,
}

enum Pending {
    Keys,
    Chunk(DataSide),
}

#[expect(clippy::large_enum_variant, reason = "one per document, moved rarely")]
enum DataState {
    Setup,
    Running {
        pending: HashMap<u64, Pending>,
        keys: Vec<String>,
        first: [Option<Chunk>; 2],
        merger: Option<Merger>,
        last: [Option<Vec<Cell>>; 2],
    },
    Done(Box<DataDiff>),
}

/// Data compare document.
pub struct DataCompare {
    pub source: Endpoint,
    pub target: Endpoint,
    pub source_table: String,
    pub target_table: String,
    /// Comma separated; empty uses the primary key.
    pub keys: String,
    pub options: DataOptions,
    lists: SchemaLists,
    state: DataState,
    error: Option<String>,
    tab: DataTab,
    grids: HashMap<u8, GridState>,
    script: ScriptView,
}

impl Default for DataCompare {
    fn default() -> Self {
        Self::new()
    }
}

impl DataCompare {
    pub fn new() -> Self {
        Self {
            source: Endpoint::default(),
            target: Endpoint::default(),
            source_table: String::new(),
            target_table: String::new(),
            keys: String::new(),
            options: DataOptions::default(),
            lists: SchemaLists::default(),
            state: DataState::Setup,
            error: None,
            tab: DataTab::Changed,
            grids: HashMap::new(),
            script: ScriptView::new(),
        }
    }

    pub fn title(&self) -> String {
        if self.source_table.is_empty() {
            "Data compare".into()
        } else {
            format!("data: {}", self.source_table.to_lowercase())
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.state, DataState::Running { .. })
    }

    pub fn result(&self) -> Option<&DataDiff> {
        match &self.state {
            DataState::Done(d) => Some(d),
            _ => None,
        }
    }

    fn sources(&self) -> (DataSource, DataSource) {
        let target_table = if self.target_table.trim().is_empty() {
            &self.source_table
        } else {
            &self.target_table
        };
        // As Oracle reads them: "my_table" as written, others in uppercase
        // (#369).
        (
            DataSource::Table {
                schema: self.source.schema.clone(),
                table: Dialect::ORACLE.typed_name(&self.source_table),
            },
            DataSource::Table {
                schema: self.target.schema.clone(),
                table: Dialect::ORACLE.typed_name(target_table),
            },
        )
    }

    /// Starts (or restarts) the compare with the current settings.
    pub fn compare(&mut self) -> CompareOutput {
        let mut out = CompareOutput::default();
        self.start(&mut out);
        out
    }

    fn start(&mut self, out: &mut CompareOutput) {
        let Some(sc) = self.source.connection.clone() else {
            return;
        };
        self.options.key_columns = self
            .keys
            .split(',')
            .map(|k| Dialect::ORACLE.typed_name(k))
            .filter(|k| !k.is_empty())
            .collect();
        let (source, _) = self.sources();
        let options = self.options.clone();
        let id = next_id();
        out.requests.push(CompareRequest {
            id,
            connection: sc,
            task: task(move |c| {
                data::prepare_session(c)?;
                data::resolve_keys(c, &source, &options)
            }),
        });
        self.error = None;
        self.state = DataState::Running {
            pending: HashMap::from([(id, Pending::Keys)]),
            keys: Vec::new(),
            first: [None, None],
            merger: None,
            last: [None, None],
        };
    }

    fn request_chunk(
        &self,
        side: DataSide,
        keys: &[String],
        after: Option<Vec<Cell>>,
        out: &mut CompareOutput,
    ) -> u64 {
        let (source, target) = self.sources();
        let (conn, src) = match side {
            DataSide::Source => (self.source.connection.clone(), source),
            DataSide::Target => (self.target.connection.clone(), target),
        };
        let (keys, filter, limit) = (
            keys.to_vec(),
            self.options.filter.clone(),
            self.options.chunk_size,
        );
        let first = after.is_none();
        let id = next_id();
        out.requests.push(CompareRequest {
            id,
            connection: conn.unwrap_or_default(),
            task: task(move |c| {
                if first {
                    data::prepare_session(c)?;
                }
                data::fetch_chunk(c, &src, &keys, &filter, after.as_deref(), limit)
            }),
        });
        id
    }

    /// Feeds a finished task back; may issue follow-up requests.
    pub fn on_task_done(&mut self, id: u64, result: TaskResult) -> (bool, Vec<CompareRequest>) {
        let Some(result) = self.lists.on_task_done(id, result) else {
            return (true, Vec::new());
        };
        let mut out = CompareOutput::default();
        let DataState::Running { pending, .. } = &mut self.state else {
            return (false, Vec::new());
        };
        let Some(kind) = pending.remove(&id) else {
            return (false, Vec::new());
        };
        if let Err(e) = self.handle(kind, result, &mut out) {
            self.error = Some(e);
            self.state = DataState::Setup;
        }
        (true, out.requests)
    }

    fn handle(
        &mut self,
        kind: Pending,
        result: TaskResult,
        out: &mut CompareOutput,
    ) -> Result<(), String> {
        match kind {
            Pending::Keys => {
                let keys = take::<Vec<String>>(result)?;
                let ids = [
                    self.request_chunk(DataSide::Source, &keys, None, out),
                    self.request_chunk(DataSide::Target, &keys, None, out),
                ];
                if let DataState::Running {
                    pending, keys: k, ..
                } = &mut self.state
                {
                    *k = keys;
                    pending.insert(ids[0], Pending::Chunk(DataSide::Source));
                    pending.insert(ids[1], Pending::Chunk(DataSide::Target));
                }
            }
            Pending::Chunk(side) => {
                let chunk = take::<Chunk>(result)?;
                let target_name = self.sources().1.target_name();
                let options = self.options.clone();
                let DataState::Running {
                    first,
                    merger,
                    last,
                    keys,
                    ..
                } = &mut self.state
                else {
                    return Ok(());
                };
                if merger.is_none() {
                    first[side as usize] = Some(chunk);
                    let (Some(s), Some(t)) = (&first[0], &first[1]) else {
                        return Ok(());
                    };
                    let mut m = Merger::new(&s.columns, &t.columns, keys, target_name, options)
                        .map_err(|e| e.to_string())?;
                    for side in [DataSide::Source, DataSide::Target] {
                        let c = first[side as usize].take().expect("both present");
                        last[side as usize] = c.rows.last().map(|r| m.key_of(side, r));
                        m.push(side, c.rows, c.done);
                    }
                    *merger = Some(m);
                } else if let Some(m) = merger.as_mut() {
                    if let Some(r) = chunk.rows.last() {
                        last[side as usize] = Some(m.key_of(side, r));
                    }
                    m.push(side, chunk.rows, chunk.done);
                }
                self.pump(out);
            }
        }
        Ok(())
    }

    /// Advances the merge and requests more rows where needed.
    fn pump(&mut self, out: &mut CompareOutput) {
        let DataState::Running {
            merger: Some(m),
            pending,
            ..
        } = &mut self.state
        else {
            return;
        };
        if !pending.is_empty() {
            // Join what's buffered; decide on new requests once the
            // in-flight chunks have arrived.
            m.advance();
            return;
        }
        let need = m.advance();
        if need.finished() {
            let DataState::Running {
                merger: Some(m), ..
            } = std::mem::replace(&mut self.state, DataState::Setup)
            else {
                unreachable!()
            };
            let diff = m.diff;
            let (source, target) = self.sources();
            let title = format!(
                "Data sync {} → {} (generated by Pythia)",
                source.target_name(),
                target.target_name()
            );
            self.script.set(diff.script(&title));
            self.tab = if diff.changed > 0 {
                DataTab::Changed
            } else if diff.only_in_source > 0 {
                DataTab::OnlyInSource
            } else if diff.only_in_target > 0 {
                DataTab::OnlyInTarget
            } else {
                DataTab::Script
            };
            self.state = DataState::Done(Box::new(diff));
            return;
        }
        let (keys, last) = match &self.state {
            DataState::Running { keys, last, .. } => (keys.clone(), last.clone()),
            _ => return,
        };
        let mut new = Vec::new();
        for (side, wanted) in [
            (DataSide::Source, need.source),
            (DataSide::Target, need.target),
        ] {
            if wanted {
                new.push((
                    self.request_chunk(side, &keys, last[side as usize].clone(), out),
                    side,
                ));
            }
        }
        if let DataState::Running { pending, .. } = &mut self.state {
            for (id, side) in new {
                pending.insert(id, Pending::Chunk(side));
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, connections: &[String]) -> CompareOutput {
        let mut out = CompareOutput::default();
        self.setup_ui(ui, connections, &mut out);
        ui.separator();
        match &self.state {
            DataState::Setup => {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.add(Icon::Table.sized(ui, pythia_ui::icons::IconSize::Large));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Compare table data").heading());
                    ui.weak(
                        "Rows are matched by key and streamed in chunks, so large tables work too.",
                    );
                    if let Some(e) = &self.error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                });
            }
            DataState::Running { merger, .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    match merger {
                        Some(m) => ui.label(format!(
                            "Comparing… {} source rows, {} target rows read · {} changed, {} only in source, {} only in target so far",
                            m.diff.source_rows, m.diff.target_rows, m.diff.changed, m.diff.only_in_source, m.diff.only_in_target
                        )),
                        None => ui.label("Reading keys and first rows…"),
                    };
                });
            }
            DataState::Done(_) => self.results_ui(ui, &mut out),
        }
        out
    }

    fn setup_ui(&mut self, ui: &mut Ui, connections: &[String], out: &mut CompareOutput) {
        let busy = self.is_busy();
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    endpoint_ui(
                        ui,
                        "dsrc",
                        "SOURCE (reference)",
                        &mut self.source,
                        connections,
                        &mut self.lists,
                        out,
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.source_table)
                            .hint_text("Table")
                            .desired_width(344.0),
                    );
                });
                ui.add(Icon::ChevronRight.mono(ui.visuals().weak_text_color()));
                ui.vertical(|ui| {
                    endpoint_ui(
                        ui,
                        "dtgt",
                        "TARGET (to update)",
                        &mut self.target,
                        connections,
                        &mut self.lists,
                        out,
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.target_table)
                            .hint_text("Table (same as source)")
                            .desired_width(344.0),
                    );
                });
            });
            ui.horizontal(|ui| {
                ui.label("Key");
                ui.add(
                    egui::TextEdit::singleline(&mut self.keys)
                        .hint_text("primary key")
                        .desired_width(160.0),
                );
                ui.label("Where");
                ui.add(
                    egui::TextEdit::singleline(&mut self.options.filter)
                        .hint_text("optional filter, e.g. dept_id = 10")
                        .desired_width(240.0),
                );
                ui.checkbox(
                    &mut self.options.include_deletes,
                    "Delete extra target rows",
                );
                let ready = self.source.connection.is_some()
                    && self.target.connection.is_some()
                    && !self.source.schema.is_empty()
                    && !self.target.schema.is_empty()
                    && !self.source_table.trim().is_empty();
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::image_and_text(
                            Icon::Execute.image(ui),
                            RichText::new("Compare").strong(),
                        ),
                    )
                    .clicked()
                {
                    self.start(out);
                }
            });
        });
    }

    fn results_ui(&mut self, ui: &mut Ui, out: &mut CompareOutput) {
        let DataState::Done(d) = &self.state else {
            return;
        };
        let mut clicked = None;
        ui.horizontal(|ui| {
            let cards = [
                (
                    d.changed,
                    "Changed",
                    status_color(ui, Status::Different),
                    Some(DataTab::Changed),
                ),
                (
                    d.only_in_source,
                    "Only in source",
                    status_color(ui, Status::OnlyInSource),
                    Some(DataTab::OnlyInSource),
                ),
                (
                    d.only_in_target,
                    "Only in target",
                    status_color(ui, Status::OnlyInTarget),
                    Some(DataTab::OnlyInTarget),
                ),
                (d.equal, "Equal", status_color(ui, Status::Equal), None),
            ];
            for (n, label, color, tab) in cards {
                if summary_card(ui, n.to_string(), label, color, tab == Some(self.tab)).clicked()
                    && let Some(tab) = tab
                {
                    clicked = Some(tab);
                }
            }
        });
        if let Some(tab) = clicked {
            self.tab = tab;
        }
        ui.horizontal(|ui| {
            ui.weak(format!(
                "Key: {} · {} source rows · {} target rows",
                d.keys.join(", "),
                d.source_rows,
                d.target_rows
            ));
            if d.is_equal() {
                ui.add(Icon::Success.image(ui));
                ui.label(RichText::new("The data is identical.").strong());
            }
        });
        for note in &d.notes {
            ui.horizontal(|ui| {
                ui.add(Icon::Info.sized(ui, pythia_ui::icons::IconSize::Small));
                ui.weak(note);
            });
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, DataTab::Changed, "Changed rows");
            ui.selectable_value(&mut self.tab, DataTab::OnlyInSource, "Only in source");
            ui.selectable_value(&mut self.tab, DataTab::OnlyInTarget, "Only in target");
            ui.selectable_value(
                &mut self.tab,
                DataTab::Script,
                format!("Sync script ({})", d.statements.len()),
            );
        });
        ui.separator();
        let sample_note = |ui: &mut Ui, shown: usize, total: u64| {
            if (shown as u64) < total {
                ui.weak(format!("Showing the first {shown} of {total} rows."));
            }
        };
        match self.tab {
            DataTab::Changed => {
                sample_note(ui, d.sample_changed.len(), d.changed);
                let (columns, rows) = changed_table(d);
                ResultGrid::new(
                    &columns,
                    &rows,
                    self.grids.entry(0).or_default(),
                    "data-changed",
                )
                .show(ui);
            }
            DataTab::OnlyInSource => {
                sample_note(ui, d.sample_only_in_source.len(), d.only_in_source);
                ResultGrid::new(
                    &d.columns,
                    &d.sample_only_in_source,
                    self.grids.entry(1).or_default(),
                    "data-only-source",
                )
                .show(ui);
            }
            DataTab::OnlyInTarget => {
                sample_note(ui, d.sample_only_in_target.len(), d.only_in_target);
                ResultGrid::new(
                    &d.columns,
                    &d.sample_only_in_target,
                    self.grids.entry(2).or_default(),
                    "data-only-target",
                )
                .show(ui);
            }
            DataTab::Script => {
                let conn = self.target.connection.clone();
                self.script.ui(ui, "data-sync-script", conn.as_deref(), out);
            }
        }
    }
}

/// Changed rows as one line per changed cell: key, column, source, target.
fn changed_table(d: &DataDiff) -> (Vec<Column>, Vec<Vec<Cell>>) {
    let text = |name: &str| Column {
        name: name.into(),
        type_name: "VARCHAR2".into(),
        numeric: false,
    };
    let columns = vec![text("KEY"), text("COLUMN"), text("SOURCE"), text("TARGET")];
    let mut rows = Vec::new();
    for c in &d.sample_changed {
        let key = c
            .key
            .iter()
            .map(|k| k.clone().unwrap_or_else(|| "NULL".into()))
            .collect::<Vec<_>>()
            .join(", ");
        for &i in &c.changed {
            rows.push(vec![
                Some(key.clone()),
                d.columns.get(i).map(|col| col.name.clone()),
                c.source[i].clone(),
                c.target[i].clone(),
            ]);
        }
    }
    (columns, rows)
}

/// Whether a name contains the (uppercase) filter, without case: a quoted
/// name keeps its lowercase letters (#305).
fn name_matches(name: &str, filter: &str) -> bool {
    filter.is_empty() || name.to_uppercase().contains(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_without_case() {
        assert!(name_matches("R62_Case", "CASE"));
        assert!(name_matches("R62_CASE", "CASE"));
        assert!(name_matches("R62_Case", ""));
        assert!(!name_matches("R62_Case", "CASES"));
    }

    #[test]
    fn ids_do_not_collide_with_host_ids() {
        assert!(next_id() >= 1 << 56);
    }

    /// The tables and key columns are taken as typed, as Oracle reads names:
    /// `"r83_cmp"` and `"Id"` as written, others in uppercase (#369).
    #[test]
    fn typed_tables_and_keys() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
        let tables = ["\"r83_cmp\"", "\"r83_cmp2\""];
        for t in tables {
            let _ = conn.execute(&format!("drop table {t} purge"), &[]);
        }
        for sql in [
            "create table \"r83_cmp\" (\"Id\" number, name varchar2(10))",
            "create table \"r83_cmp2\" (\"Id\" number, name varchar2(10))",
            "insert into \"r83_cmp\" values (1, 'a')",
            "insert into \"r83_cmp\" values (2, 'b')",
            "insert into \"r83_cmp2\" values (1, 'x')",
            "commit",
        ] {
            conn.execute(sql, &[]).unwrap();
        }
        let endpoint = || Endpoint {
            connection: Some("test".into()),
            schema: user.to_uppercase(),
        };
        let mut d = DataCompare::new();
        d.source = endpoint();
        d.target = endpoint();
        d.source_table = "\"r83_cmp\"".into();
        d.target_table = " \"r83_cmp2\" ".into();
        d.keys = "\"Id\"".into();
        let mut queue: std::collections::VecDeque<CompareRequest> = d.compare().requests.into();
        // As a host runs them; those of a compare that failed are no
        // longer its.
        while let Some(request) = queue.pop_front() {
            let (_, more) = d.on_task_done(request.id, (request.task)(&conn));
            queue.extend(more);
        }
        for t in tables {
            conn.execute(&format!("drop table {t} purge"), &[]).unwrap();
        }
        let diff = d.result().unwrap_or_else(|| panic!("{:?}", d.error));
        assert_eq!(
            (
                diff.equal,
                diff.changed,
                diff.only_in_source,
                diff.only_in_target
            ),
            (0, 1, 1, 0)
        );
    }

    #[test]
    fn task_results_roundtrip() {
        let boxed: TaskResult = Ok(Box::new(vec!["A".to_owned()]));
        assert_eq!(take::<Vec<String>>(boxed).unwrap(), ["A"]);
        let wrong: TaskResult = Ok(Box::new(1u8));
        assert!(take::<Vec<String>>(wrong).is_err());
    }

    #[test]
    fn unknown_ids_are_not_consumed() {
        let mut s = SchemaCompare::new();
        assert!(!s.on_task_done(42, Ok(Box::new(()))));
        let mut d = DataCompare::new();
        assert!(!d.on_task_done(42, Ok(Box::new(()))).0);
    }

    #[test]
    fn changed_rows_flatten() {
        let mut d = DataDiff {
            columns: vec![
                Column {
                    name: "ID".into(),
                    type_name: "NUMBER".into(),
                    numeric: true,
                },
                Column {
                    name: "N".into(),
                    type_name: "VARCHAR2".into(),
                    numeric: false,
                },
            ],
            ..DataDiff::default()
        };
        d.sample_changed.push(crate::data::ChangedRow {
            key: vec![Some("1".into())],
            source: vec![Some("1".into()), Some("a".into())],
            target: vec![Some("1".into()), Some("b".into())],
            changed: vec![1],
        });
        let (cols, rows) = changed_table(&d);
        assert_eq!(cols.len(), 4);
        assert_eq!(
            rows,
            [vec![
                Some("1".into()),
                Some("N".into()),
                Some("a".into()),
                Some("b".into())
            ]]
        );
    }
}
