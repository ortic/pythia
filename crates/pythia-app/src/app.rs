use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use egui::{Color32, RichText, Ui};
use egui_dock::{DockArea, DockState, NodeIndex, TabStyle, TabViewer};
use pythia_config::{
    ConnectionProfile, Connections as SavedConnections, Paths, Settings, ThemeChoice, load_toml,
    save_toml,
};
use pythia_db::{DbEvent, DbRequest, Dialect, Feature, MetaQuery, MetaResult};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::browser::Browser;
use crate::commands::{Command, Keymap};
use crate::connections::{Connection, Connections, Notify, Passwords, SessionKey};
use crate::dialogs::{
    Answer, CommandPalette, Confirm, ConfirmAction, ConnectDialog, FinderAction, HistoryDialog,
    ObjectFinder, confirm_ui, opens_as_program,
};
use crate::documents::{
    DocContext, DocId, DocKind, Document, Effects, ObjectRef, OpenRequest, Target, request_id,
};
use crate::history::History;
use crate::object_view::ObjectView;
use crate::program_window::ProgramWindow;
use crate::sql_window::SqlWindow;
use crate::{secrets, theme};
use pythia_ui::icons::{Icon, icon_label};

// Named before the app became Pythia; kept so saved workspaces still load.
const WORKSPACE_KEY: &str = "orui-workspace-v2";

/// Why *View → Variables* has nothing to show (#168).
const NO_VARIABLES: &str = "This SQL window has no bind (:name) or substitution (&name) variables";
const NO_SQL_WINDOW: &str = "The Variables panel belongs to a SQL window";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileAction {
    Open,
    SaveAs(DocId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tab {
    Browser,
    Project,
    Log,
    /// The debugger's controls and call stack; its other panels are tabs
    /// of their own, to arrange freely (#163).
    Debugger,
    Doc(DocId),
    Assistant,
    DebugVariables,
    DebugWatches,
    DebugBreakpoints,
    DebugSql,
}

/// The debugger's panels, docked right of each other below the documents
/// (breakpoints behind watches), each with the share the panel left of it
/// keeps.
const DEBUG_PANELS: [(Tab, f32); 4] = [
    (Tab::Debugger, 0.5),
    (Tab::DebugVariables, 0.3),
    (Tab::DebugWatches, 0.55),
    (Tab::DebugBreakpoints, 0.5),
];

/// A lookup of the debugger's SQL panel in flight.
enum DebugSqlLookup {
    /// The statements the session ran, at this stop.
    Find(u64),
    /// A statement's plan: SQL_ID and child.
    Plan(String, i64),
}

/// Layout and documents, restored on the next start.
#[derive(Serialize, Deserialize)]
struct Workspace {
    dock: DockState<Tab>,
    docs: BTreeMap<DocId, Document>,
    next_doc: u64,
    last_profile: Option<ConnectionProfile>,
    #[serde(default)]
    project: crate::project::Project,
    #[serde(default)]
    marks: crate::browser::Marks,
    /// Breakpoints, watches and Break on exceptions.
    #[serde(default)]
    debug: pythia_debugger::ui::Saved,
    /// The debugger's panels are tabs of their own (#163): before, the
    /// Debugger tab had them all.
    #[serde(default)]
    debug_panels: bool,
}

pub struct PythiaApp {
    paths: Option<Paths>,
    settings: Settings,
    saved: SavedConnections,
    keymap: Keymap,
    /// The code and interface font files installed.
    loaded_fonts: Option<(String, String)>,
    notify: Notify,

    dock: DockState<Tab>,
    docs: BTreeMap<DocId, Document>,
    next_doc: u64,
    /// The document commands act on: the focused one, or the last focused.
    focused: Option<DocId>,

    conns: Connections,
    browser: Browser,
    project: crate::project::Project,
    /// Main-session requests issued by documents, routed back by id.
    routes: HashMap<u64, DocId>,
    last_profile: Option<ConnectionProfile>,
    /// Connection the connect dialog is currently opening.
    connecting: Option<String>,
    /// The active connection: the toolbar, object browser, status bar, new
    /// windows and SQL windows use it, unless windows switch connections on
    /// their own (Settings → Query). Program windows and object views keep
    /// the connection they were opened on.
    active: Option<String>,
    /// `Settings::switch_window_connection` as of the last frame: turning it
    /// off brings the SQL windows back to one connection.
    windows_switched: bool,
    /// SSH host keys trusted in Pythia.
    known_hosts: PathBuf,

    history: History,
    user_reports: crate::reports::UserReports,
    /// "Save as report" dialog: name, category, SQL.
    save_report: Option<(String, String, String)>,
    debug: pythia_debugger::ui::DebugSession,
    /// The running debug call: (document, task request id).
    debug_run: Option<(DocId, u64)>,
    /// The connection of the last debug run, for the setup window.
    debug_connection: Option<String>,
    debug_setup: crate::debug_setup::DebugSetup,
    /// Commands asked for by documents, run with the next frame's commands.
    queued_commands: Vec<Command>,
    assistant: crate::ai::Assistant,
    /// Dictionary caches for code assistance, by connection.
    catalogs: HashMap<String, pythia_lsp_core::CatalogCache>,
    catalog_revision: u64,
    /// Catalog task request id → connection.
    catalog_inflight: HashMap<u64, String>,
    /// Reading a schema's source to learn formatter settings from: request id.
    learn_inflight: Option<u64>,
    /// Statements the formatter keeps as written, being laid out by the AI.
    ai_format: Option<AiFormatRun>,
    /// Invalid objects per connection, for the status bar.
    invalid: HashMap<String, InvalidCount>,
    /// Count requests in flight: request id → connection.
    invalid_inflight: HashMap<u64, String>,
    /// Connections whose objects changed while a count was in flight, to
    /// count again once it is in.
    invalid_again: std::collections::HashSet<String>,
    /// Lookups of why debugger variables are opaque: request id → unit.
    opaque_inflight: HashMap<u64, pythia_debugger::Unit>,
    /// The debugged session saying who it is (request id).
    debug_session_task: Option<u64>,
    /// Lookups for the debugger's SQL panel: request id → what.
    debug_sql_inflight: HashMap<u64, DebugSqlLookup>,
    /// Requests already sent (or answered) per connection, to avoid repeats.
    catalog_requested: std::collections::HashSet<(String, pythia_lsp_core::CatalogRequest)>,
    log: Vec<String>,
    connect_dialog: ConnectDialog,
    palette: CommandPalette,
    symbol_picker: crate::dialogs::SymbolPicker,
    reference_list: crate::dialogs::ReferenceList,
    /// Places jumped from, for Back and Forward.
    nav: NavHistory,
    /// The user confirmed running changes on production (one execution).
    production_confirmed: bool,
    /// Recently closed documents, for reopening (newest last).
    closed_docs: Vec<Document>,
    /// Native file dialog running on a helper thread.
    file_dialog: Option<(
        std::sync::mpsc::Receiver<Option<std::path::PathBuf>>,
        FileAction,
    )>,
    settings_dialog: crate::settings_dialog::SettingsDialog,
    finder: ObjectFinder,
    history_dialog: HistoryDialog,
    confirm: Option<Confirm>,
    about_open: bool,
    /// Exit was confirmed; don't ask again on the next close request.
    quit_confirmed: bool,
}

impl PythiaApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Option<Paths>) -> Self {
        let settings: Settings = paths
            .as_ref()
            .map(|p| load_or_default(&p.settings_file()))
            .unwrap_or_default();
        let saved: SavedConnections = paths
            .as_ref()
            .map(|p| load_or_default(&p.connections_file()))
            .unwrap_or_default();
        let history: History = paths
            .as_ref()
            .map(|p| load_or_default(&history_file(p)))
            .unwrap_or_default();
        let user_reports: crate::reports::UserReports = paths
            .as_ref()
            .map(|p| load_or_default(&reports_file(p)))
            .unwrap_or_default();
        let workspace: Option<Workspace> =
            cc.storage.and_then(|s| eframe::get_value(s, WORKSPACE_KEY));
        let known_hosts = paths.as_ref().map_or_else(
            || std::env::temp_dir().join("pythia-known_hosts"),
            Paths::known_hosts_file,
        );

        let mut app = Self {
            paths,
            keymap: Keymap::with_overrides(settings.keymap, &settings.keybindings),
            loaded_fonts: None,
            settings,
            saved,
            notify: Notify(cc.egui_ctx.clone()),
            dock: DockState::new(vec![]),
            docs: BTreeMap::new(),
            next_doc: 1,
            focused: None,
            conns: Connections::default(),
            browser: Browser::default(),
            project: Default::default(),
            routes: HashMap::new(),
            last_profile: None,
            connecting: None,
            active: None,
            windows_switched: false,
            known_hosts,
            history,
            user_reports,
            save_report: None,
            debug: Default::default(),
            debug_run: None,
            debug_connection: None,
            debug_setup: Default::default(),
            queued_commands: Vec::new(),
            assistant: Default::default(),
            catalogs: HashMap::new(),
            catalog_revision: 0,
            catalog_inflight: HashMap::new(),
            learn_inflight: None,
            ai_format: None,
            invalid: HashMap::new(),
            invalid_inflight: HashMap::new(),
            invalid_again: std::collections::HashSet::new(),
            opaque_inflight: HashMap::new(),
            debug_session_task: None,
            debug_sql_inflight: HashMap::new(),
            catalog_requested: std::collections::HashSet::new(),
            log: Vec::new(),
            connect_dialog: ConnectDialog::default(),
            palette: CommandPalette::default(),
            symbol_picker: Default::default(),
            reference_list: Default::default(),
            nav: NavHistory::default(),
            production_confirmed: false,
            closed_docs: Vec::new(),
            file_dialog: None,
            settings_dialog: Default::default(),
            finder: ObjectFinder::default(),
            history_dialog: HistoryDialog::default(),
            confirm: None,
            about_open: false,
            quit_confirmed: false,
        };
        if let Some(ws) = workspace {
            // Kept also when no document was open (#118).
            app.last_profile = ws.last_profile;
            app.project = ws.project;
            app.browser.marks = ws.marks;
            app.debug.restore(ws.debug);
            if !ws.docs.is_empty() {
                app.dock = ws.dock;
                app.docs = ws.docs;
                app.next_doc = ws.next_doc;
                let docs = &app.docs;
                app.dock
                    .retain_tabs(|tab| !matches!(tab, Tab::Doc(id) if !docs.contains_key(id)));
                if !ws.debug_panels && app.dock.find_tab(&Tab::Debugger).is_some() {
                    app.add_debug_panels();
                }
            }
        }
        if app.docs.is_empty() {
            app.reset_layout();
        }
        app.focused = app.docs.keys().next().copied();

        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        theme::install(&cc.egui_ctx);
        app.apply_settings(&cc.egui_ctx);
        // Nothing is connected yet, and there's little to do without a
        // connection (#132).
        app.connect_dialog.show(app.last_profile.as_ref());
        app.log("Pythia started");
        app
    }

    fn reset_layout(&mut self) {
        if self.docs.is_empty() {
            let id = self.new_doc_id();
            self.docs.insert(
                id,
                Document {
                    connection: None,
                    kind: DocKind::Sql(SqlWindow::new("SQL 1".into(), String::new())),
                },
            );
        }
        let tabs: Vec<Tab> = self.docs.keys().map(|id| Tab::Doc(*id)).collect();
        let mut dock = DockState::new(tabs);
        let surface = dock.main_surface_mut();
        surface.split_left(NodeIndex::root(), 0.24, vec![Tab::Browser, Tab::Project]);
        self.dock = dock;
    }

    fn new_doc_id(&mut self) -> DocId {
        let id = DocId(self.next_doc);
        self.next_doc += 1;
        id
    }

    fn apply_settings(&mut self, ctx: &egui::Context) {
        let fonts = (
            self.settings.editor.font_file.trim().to_owned(),
            self.settings.appearance.font_file.trim().to_owned(),
        );
        if self.loaded_fonts.as_ref() != Some(&fonts) {
            let code = self.read_font(&fonts.0);
            let ui = self.read_font(&fonts.1);
            theme::set_fonts(ctx, code, ui);
            self.loaded_fonts = Some(fonts);
        }
        theme::apply(ctx, &self.settings.appearance, &self.settings.grid);
        ctx.set_theme(match self.settings.theme {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::Light => egui::ThemePreference::Light,
        });
        ctx.set_zoom_factor(self.settings.ui_scale);
        self.sync_autocommit();
    }

    /// The bytes of a font file; `None` for an empty path or a file that
    /// can't be used, which is logged.
    fn read_font(&mut self, path: &str) -> Option<Vec<u8>> {
        if path.is_empty() {
            return None;
        }
        match std::fs::read(path) {
            Ok(bytes) if is_font(&bytes) => Some(bytes),
            Ok(_) => {
                self.log(format!("{path} is not a TrueType or OpenType font"));
                None
            }
            Err(e) => {
                self.log(format!("Could not load font {path}: {e}"));
                None
            }
        }
    }

    fn save_user_reports(&self) {
        if let Some(paths) = &self.paths
            && let Err(e) = save_toml(&reports_file(paths), &self.user_reports)
        {
            error!("saving reports: {e:#}");
        }
    }

    /// The "Save as report" dialog.
    fn save_report_ui(&mut self, ctx: &egui::Context) {
        let Some((name, category, sql)) = &mut self.save_report else {
            return;
        };
        let mut open = true;
        let mut save = false;
        egui::Window::new("Save as report")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                egui::Grid::new("save_report")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        let r = ui.add(egui::TextEdit::singleline(name).desired_width(280.0));
                        if name.is_empty() && ui.memory(|m| m.focused().is_none()) {
                            r.request_focus();
                        }
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            save = true;
                        }
                        ui.end_row();
                        ui.label("Category");
                        ui.add(
                            egui::TextEdit::singleline(category)
                                .hint_text("optional, e.g. Sales")
                                .desired_width(280.0),
                        );
                        ui.end_row();
                    });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Bind variables (:name) and &substitutions become parameters, \
                         asked for when the report runs.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
                let first = sql
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or_default();
                ui.label(RichText::new(first).monospace().weak());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!name.trim().is_empty(), egui::Button::new("Save"))
                        .clicked()
                    {
                        save = true;
                    }
                });
            });
        if save && !name.trim().is_empty() {
            let report = crate::reports::UserReport {
                name: name.trim().to_owned(),
                category: category.trim().to_owned(),
                sql: sql.clone(),
            };
            self.log(format!("Saved report {}", report.name));
            self.user_reports.save(report);
            self.save_user_reports();
            self.save_report = None;
        } else if !open {
            self.save_report = None;
        }
    }

    fn save_settings(&self) {
        if let Some(paths) = &self.paths
            && let Err(e) = save_toml(&paths.settings_file(), &self.settings)
        {
            error!("saving settings: {e:#}");
        }
    }

    fn save_connections(&self) {
        if let Some(paths) = &self.paths
            && let Err(e) = save_toml(&paths.connections_file(), &self.saved)
        {
            error!("saving connections: {e:#}");
        }
    }

    fn save_history(&mut self) {
        if !self.history.dirty {
            return;
        }
        if let Some(paths) = &self.paths {
            match save_toml(&history_file(paths), &self.history) {
                Ok(()) => self.history.dirty = false,
                Err(e) => error!("saving history: {e:#}"),
            }
        }
    }

    fn log(&mut self, line: impl Into<String>) {
        let line = line.into();
        info!("{line}");
        self.log.push(line);
    }

    /// The connection new documents and commands use when nothing is focused.
    fn current_connection(&self) -> Option<String> {
        let open = |c: &String| self.conns.get(c).is_some();
        let current = if self.settings.switch_window_connection {
            // The focused window's.
            self.focused
                .and_then(|id| self.docs.get(&id))
                .and_then(|d| d.connection.clone())
                .filter(open)
                .or_else(|| self.browser.connection.clone())
        } else {
            self.active.clone().filter(open)
        };
        current.or_else(|| self.conns.open.keys().next().cloned())
    }

    /// Makes `name` the active connection: the object browser shows it and
    /// the SQL windows move to it, each with a new session. Windows with a
    /// transaction or a running statement are asked about first. With
    /// windows switching connections on their own, only the browser moves.
    fn activate(&mut self, name: String) {
        if self.conns.get(&name).is_none() {
            return;
        }
        if self.settings.switch_window_connection {
            self.attach_browser(name);
            return;
        }
        let asked: Vec<DocId> = self
            .moving_windows(&name)
            .into_iter()
            .filter(|id| self.has_pending_tx(*id) || self.docs[id].is_busy())
            .collect();
        if asked.is_empty() {
            self.switch_to(name, None);
            return;
        }
        let names: Vec<String> = asked
            .iter()
            .filter_map(|id| self.docs.get(id))
            .map(Document::title)
            .collect();
        let windows = match names.as_slice() {
            [one] => format!("{one} has"),
            many => format!("{} have", many.join(", ")),
        };
        self.confirm = Some(Confirm {
            action: ConfirmAction::Activate(name.clone()),
            title: format!("Switch to {name}"),
            text: format!(
                "{windows} a transaction or a running statement. The SQL windows move to \
                 {name}: their transactions end once what's running has finished."
            ),
            yes: "Commit and switch",
            other: Some("Roll back and switch"),
        });
    }

    /// SQL windows not on `name`.
    fn moving_windows(&self, name: &str) -> Vec<DocId> {
        self.docs
            .iter()
            .filter(|(_, d)| {
                matches!(d.kind, DocKind::Sql(_)) && d.connection.as_deref() != Some(name)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Switches the active connection to `name`, committing (`Some(true)`)
    /// or rolling back the transactions of the windows that move.
    fn switch_to(&mut self, name: String, end: Option<bool>) {
        self.active = Some(name.clone());
        self.attach_browser(name.clone());
        let dialect = self.dialect_of(Some(&name));
        for id in self.moving_windows(&name) {
            match end {
                Some(commit) => self.conns.end_doc(id, commit),
                None => self.conns.close_doc(id),
            }
            let Some(doc) = self.docs.get_mut(&id) else {
                continue;
            };
            if let DocKind::Sql(w) = &mut doc.kind {
                w.state.running = None;
                w.state.fetching = None;
                w.state.queue.clear();
            }
            doc.connection = Some(name.clone());
            let effects = doc.on_attach(dialect);
            self.apply(Some(id), effects);
        }
    }

    /// Makes `database` the connection's (MySQL): its sessions run in it and
    /// completion looks there.
    fn use_database(&mut self, connection: &str, database: &str) {
        let Some(conn) = self.conns.get_mut(connection) else {
            return;
        };
        conn.use_database(database, &self.notify);
        let names = name_style(conn.dialect());
        self.catalogs.insert(
            connection.to_owned(),
            pythia_lsp_core::CatalogCache::new(database.to_owned()).with_names(names),
        );
        self.catalog_requested.retain(|(c, _)| c != connection);
        self.log(format!("{connection}: database {database}"));
    }

    /// Shows a connection in the object browser.
    fn attach_browser(&mut self, connection: String) {
        if self.browser.connection.as_deref() == Some(connection.as_str()) {
            return;
        }
        let schema = self
            .conns
            .get(&connection)
            .map(|c| c.current_schema())
            .unwrap_or_default();
        let dialect = self.dialect_of(Some(&connection));
        let effects = self.browser.attach(connection, schema, dialect);
        self.apply_browser(effects);
    }

    /// The current connection's database (Oracle without a connection):
    /// what menus, toolbar and palette offer.
    fn dialect(&self) -> Dialect {
        self.dialect_of(self.current_connection().as_deref())
    }

    /// The database of a connection by name.
    fn dialect_of(&self, connection: Option<&str>) -> Dialect {
        connection
            .and_then(|c| self.conns.get(c))
            .map(Connection::dialect)
            .unwrap_or_default()
    }

    /// Whether a connection's profile is read-only.
    fn read_only_of(&self, connection: Option<&str>) -> bool {
        connection
            .and_then(|c| self.conns.get(c))
            .is_some_and(|c| c.profile.read_only)
    }

    /// Whether a connection's profile is marked as production.
    fn production_of(&self, connection: Option<&str>) -> bool {
        connection
            .and_then(|c| self.conns.get(c))
            .is_some_and(|c| c.profile.production)
    }

    fn doc_context(&self, id: DocId) -> DocContext<'_> {
        let connected = self
            .docs
            .get(&id)
            .and_then(|d| d.connection.as_deref())
            .and_then(|c| self.conns.get(c))
            .is_some_and(|c| c.is_connected());
        DocContext {
            id,
            settings: &self.settings,
            connected,
            fetch_size: self.settings.fetch_size,
            connection: self.docs.get(&id).and_then(|d| d.connection.clone()),
            catalog: None,
            catalog_revision: self.catalog_revision,
            connections: &[],
            debug: Some(&self.debug),
            marks: None,
            privilege: None,
            dialect: self.dialect_of(self.docs.get(&id).and_then(|d| d.connection.as_deref())),
            read_only: self.read_only_of(self.docs.get(&id).and_then(|d| d.connection.as_deref())),
            production: self
                .production_of(self.docs.get(&id).and_then(|d| d.connection.as_deref())),
        }
    }

    // ----- documents ------------------------------------------------------

    fn add_doc(&mut self, connection: Option<String>, kind: DocKind) -> DocId {
        let id = self.new_doc_id();
        let dialect = self.dialect_of(connection.as_deref());
        let mut doc = Document { connection, kind };
        let effects = doc.on_attach(dialect);
        self.docs.insert(id, doc);
        self.open_tab(Tab::Doc(id));
        self.focused = Some(id);
        self.apply(Some(id), effects);
        id
    }

    fn new_sql_title(&self) -> String {
        let n = self
            .docs
            .values()
            .filter(|d| matches!(d.kind, DocKind::Sql(_)))
            .count()
            + 1;
        format!("SQL {n}")
    }

    fn open(&mut self, request: OpenRequest) {
        // Without program windows and editors, objects open in an object view.
        let request = match request {
            OpenRequest::Program { connection, object }
            | OpenRequest::ProgramAt {
                connection, object, ..
            }
            | OpenRequest::ProgramMember {
                connection, object, ..
            } if !self.dialect_of(Some(&connection)).supports(Feature::PlSql) => {
                OpenRequest::Object { connection, object }
            }
            OpenRequest::EditTable {
                connection, object, ..
            } if !self
                .dialect_of(Some(&connection))
                .supports(Feature::ObjectEditors) =>
            {
                OpenRequest::Object { connection, object }
            }
            request => request,
        };
        if let OpenRequest::Program { connection, object }
        | OpenRequest::ProgramAt {
            connection, object, ..
        }
        | OpenRequest::Object { connection, object }
        | OpenRequest::EditTable {
            connection, object, ..
        } = &request
        {
            self.browser.marks.touch(connection, object.clone());
        }
        // A SQL window for another connection: the app moves there.
        if let OpenRequest::Sql { connection, .. }
        | OpenRequest::Debug { connection, .. }
        | OpenRequest::File {
            connection: Some(connection),
            ..
        } = &request
            && !self.settings.switch_window_connection
            && self.active.as_ref() != Some(connection)
        {
            self.activate(connection.clone());
        }
        match request {
            OpenRequest::File { path, connection } => self.open_file(path, connection),
            OpenRequest::Sql {
                connection,
                text,
                run,
            } => {
                let title = self.new_sql_title();
                let id = self.add_doc(Some(connection), DocKind::Sql(SqlWindow::new(title, text)));
                if run {
                    self.execute(id, false);
                }
            }
            OpenRequest::Debug {
                connection,
                text,
                entry,
            } => {
                let mut window = SqlWindow::new(self.new_sql_title(), text);
                window.state.debug_entry = Some(entry);
                // Debugs the block like Debug in the window (asking for
                // its binds first).
                let effects = window.debug_current();
                let id = self.add_doc(Some(connection), DocKind::Sql(window));
                self.apply(Some(id), effects);
            }
            OpenRequest::Program { connection, object } => {
                if !self.focus_existing(&connection, &object, true) {
                    self.add_doc(
                        Some(connection),
                        DocKind::Program(ProgramWindow::new(object)),
                    );
                }
            }
            OpenRequest::Maintenance {
                connection,
                object,
                task,
            } => {
                let existing = self.docs.iter_mut().find_map(|(id, d)| match &mut d.kind {
                    DocKind::Maintenance(w)
                        if d.connection.as_deref() == Some(connection.as_str())
                            && w.object == object =>
                    {
                        // A finding's fix picks its task there too.
                        if let Some((task, options)) = task.clone() {
                            w.task = Some(task);
                            w.options = options;
                        }
                        Some(*id)
                    }
                    _ => None,
                });
                match existing {
                    Some(id) => self.open_tab(Tab::Doc(id)),
                    None => {
                        let mut window = crate::maintenance::MaintenanceWindow::new(object);
                        if let Some((task, options)) = task {
                            window = window.with_task(task, options);
                        }
                        self.add_doc(Some(connection), DocKind::Maintenance(window));
                    }
                }
            }
            OpenRequest::MaintenanceScan { connection, schema } => {
                let existing = self.docs.iter().find_map(|(id, d)| match &d.kind {
                    DocKind::MaintenanceScan(w)
                        if d.connection.as_deref() == Some(connection.as_str())
                            && w.schema == schema =>
                    {
                        Some(*id)
                    }
                    _ => None,
                });
                match existing {
                    Some(id) => self.open_tab(Tab::Doc(id)),
                    None => {
                        self.add_doc(
                            Some(connection),
                            DocKind::MaintenanceScan(crate::maintenance::ScanWindow::new(schema)),
                        );
                    }
                }
            }
            OpenRequest::InvalidObjects {
                connection,
                schema,
                recompile,
            } => {
                let existing = self.docs.iter_mut().find_map(|(id, d)| match &mut d.kind {
                    DocKind::Invalid(w)
                        if d.connection.as_deref() == Some(connection.as_str())
                            && w.schema == schema =>
                    {
                        w.recompile_on_load |= recompile;
                        Some(*id)
                    }
                    _ => None,
                });
                match existing {
                    Some(id) => {
                        self.open_tab(Tab::Doc(id));
                        if recompile && let Some(d) = self.docs.get_mut(&id) {
                            let effects = match &mut d.kind {
                                DocKind::Invalid(w) => w.refresh(),
                                _ => Effects::default(),
                            };
                            self.apply(Some(id), effects);
                        }
                    }
                    None => {
                        self.add_doc(
                            Some(connection),
                            DocKind::Invalid(crate::invalid_window::InvalidObjectsWindow::new(
                                schema, recompile,
                            )),
                        );
                    }
                }
            }
            OpenRequest::ProgramMember {
                connection,
                object,
                member,
                parameter,
            } => {
                self.open(OpenRequest::Program { connection, object });
                if let Some(DocKind::Program(w)) = self
                    .focused
                    .and_then(|id| self.docs.get_mut(&id))
                    .map(|d| &mut d.kind)
                {
                    w.goto_member(&member, parameter.as_deref());
                }
            }
            OpenRequest::Object { connection, object } => {
                if !self.focus_existing(&connection, &object, false) {
                    let dialect = self.dialect_of(Some(&connection));
                    self.add_doc(
                        Some(connection),
                        DocKind::Object(ObjectView::new(object, dialect, &self.notify.0)),
                    );
                }
            }
            OpenRequest::Monitor { connection, start } => {
                let existing = self.docs.iter().find_map(|(id, d)| {
                    matches!(&d.kind, DocKind::Monitor(m) if m.start == start)
                        .then_some(*id)
                        .filter(|_| d.connection.as_deref() == Some(connection.as_str()))
                });
                match existing {
                    Some(id) => self.open_tab(Tab::Doc(id)),
                    None => {
                        self.add_doc(
                            Some(connection),
                            DocKind::Monitor(crate::monitor_window::MonitorWindow::new(start)),
                        );
                    }
                }
            }
            OpenRequest::ConnectDialog => self.connect_dialog.show(self.last_profile.as_ref()),
            OpenRequest::EditObject { connection, target } => {
                let existing = self.docs.iter().find_map(|(id, d)| match &d.kind {
                    DocKind::ObjectEditor(e)
                        if e.target == target
                            && d.connection.as_deref() == Some(connection.as_str())
                            && !matches!(target, crate::object_editor::EditTarget::New { .. }) =>
                    {
                        Some(*id)
                    }
                    _ => None,
                });
                match existing {
                    Some(id) => self.open_tab(Tab::Doc(id)),
                    None => {
                        self.add_doc(
                            Some(connection),
                            DocKind::ObjectEditor(crate::object_editor::ObjectEditor::new(target)),
                        );
                    }
                }
            }
            OpenRequest::EditTable {
                connection,
                object,
                tab,
            } => {
                self.add_doc(
                    Some(connection),
                    DocKind::TableEditor(
                        crate::table_editor::TableEditor::new(object).with_tab(tab),
                    ),
                );
            }
            OpenRequest::NewTable {
                connection,
                owner,
                like,
            } => {
                self.add_doc(
                    Some(connection),
                    DocKind::TableEditor(crate::table_editor::TableEditor::create(owner, like)),
                );
            }
            OpenRequest::NewProgram {
                connection,
                object,
                sources,
            } => {
                self.add_doc(
                    Some(connection),
                    DocKind::Program(ProgramWindow::with_sources(object, sources)),
                );
            }
            OpenRequest::InsertText { text } => {
                match self
                    .focused
                    .and_then(|id| self.docs.get_mut(&id))
                    .and_then(Document::sql_window)
                {
                    Some(w) => w.editor.insert(&text),
                    None => {
                        if let Some(connection) = self.current_connection() {
                            self.open(OpenRequest::Sql {
                                connection,
                                text,
                                run: false,
                            });
                        }
                    }
                }
            }
            OpenRequest::ProgramAt {
                connection,
                object,
                line,
            } => {
                let object_type = object.object_type;
                self.open(OpenRequest::Program { connection, object });
                if let Some(DocKind::Program(w)) = self
                    .focused
                    .and_then(|id| self.docs.get_mut(&id))
                    .map(|d| &mut d.kind)
                {
                    w.goto_line(object_type, line);
                }
            }
            OpenRequest::Browse { connection } => self.activate(connection),
        }
    }

    /// Focuses an open program window / object view for the object, if any.
    fn focus_existing(&mut self, connection: &str, object: &ObjectRef, program: bool) -> bool {
        let found = self.docs.iter().find_map(|(id, d)| {
            let same = match &d.kind {
                DocKind::Program(w) if program => {
                    w.object.owner == object.owner
                        && w.object.name == object.name
                        && w.parts.iter().any(|p| p.object_type == object.object_type)
                }
                DocKind::Object(v) if !program => v.object == *object,
                _ => false,
            };
            (same && d.connection.as_deref() == Some(connection)).then_some(*id)
        });
        if let Some(id) = found {
            self.open_tab(Tab::Doc(id));
            self.focused = Some(id);
            if let Some(DocKind::Program(w)) = self.docs.get_mut(&id).map(|d| &mut d.kind) {
                w.active = w
                    .parts
                    .iter()
                    .position(|p| p.object_type == object.object_type)
                    .unwrap_or(w.active);
            }
        }
        found.is_some()
    }

    /// Shows the tab (adding it if needed) and gives it the dock's focus, so
    /// the next frame's "focused document" is this one.
    fn open_tab(&mut self, tab: Tab) {
        if self.dock.find_tab(&tab).is_none() {
            self.place_tab(tab);
        }
        if let Some(path) = self.dock.find_tab(&tab) {
            self.dock.set_focused_node_and_surface(path.node_path());
            let _ = self.dock.set_active_tab(path);
        }
        if let Tab::Doc(id) = tab {
            self.focused = Some(id);
        }
    }

    /// Shows the debugger with any of its panels that were closed. The SQL
    /// plan comes with it while shown.
    fn open_debugger(&mut self) {
        self.add_debug_panels();
        self.open_tab(Tab::Debugger);
    }

    fn add_debug_panels(&mut self) {
        for (tab, _) in DEBUG_PANELS {
            if self.dock.find_tab(&tab).is_none() {
                self.place_tab(tab);
            }
        }
        if self.debug.sql_shown() && self.dock.find_tab(&Tab::DebugSql).is_none() {
            self.place_tab(Tab::DebugSql);
        }
    }

    /// The SQL plan's tab follows its toggle: it opens, comes to the front
    /// on a misestimate, and goes when hidden.
    fn sync_debug_sql(&mut self) {
        let raise = self.debug.take_sql_raise();
        let tab = self.dock.find_tab(&Tab::DebugSql);
        if !self.debug.sql_shown() {
            if let Some(path) = tab {
                self.dock.remove_tab(path);
            }
        } else if raise || tab.is_none() {
            self.open_tab(Tab::DebugSql);
        }
    }

    fn place_tab(&mut self, tab: Tab) {
        // The debugger docks below the documents so code and variables are
        // visible together.
        if tab == Tab::Debugger
            && let Some(path) = self.dock.find_tab_from(|t| matches!(t, Tab::Doc(_)))
        {
            self.dock[path.surface].split_below(path.node, 0.56, vec![tab]);
            return;
        }
        if let Some(i) = DEBUG_PANELS.iter().position(|(t, _)| *t == tab) {
            let before = DEBUG_PANELS[..i]
                .iter()
                .rev()
                .find_map(|(t, _)| self.dock.find_tab(t).map(|path| (*t, path)));
            match before {
                Some((Tab::DebugWatches, path)) if tab == Tab::DebugBreakpoints => {
                    self.dock.set_focused_node_and_surface(path.node_path());
                    self.dock.push_to_focused_leaf(tab);
                    // Watches stay in front.
                    let _ = self.dock.set_active_tab(path);
                    return;
                }
                Some((_, path)) => {
                    self.dock[path.surface].split_right(path.node, DEBUG_PANELS[i].1, vec![tab]);
                    return;
                }
                None => {}
            }
        }
        // The SQL plan needs width and height: right of the code it ran.
        if tab == Tab::DebugSql
            && let Some(path) = self.dock.find_tab_from(|t| matches!(t, Tab::Doc(_)))
        {
            self.dock[path.surface].split_right(path.node, 0.5, vec![tab]);
            return;
        }
        // The assistant sits right of the documents it works with.
        if tab == Tab::Assistant
            && let Some(path) = self.dock.find_tab_from(|t| matches!(t, Tab::Doc(_)))
        {
            self.dock[path.surface].split_right(path.node, 0.66, vec![tab]);
            return;
        }
        // The project sits next to the object browser.
        if tab == Tab::Project
            && let Some(path) = self.dock.find_tab(&Tab::Browser)
        {
            self.dock.set_focused_node_and_surface(path.node_path());
            self.dock.push_to_focused_leaf(tab);
            return;
        }
        if !matches!(tab, Tab::Doc(_)) {
            self.dock.push_to_focused_leaf(tab);
            return;
        }
        // Documents go next to the focused document, or any document, never
        // into the browser/log pane.
        let focused = self.focused.map(Tab::Doc);
        let near = focused
            .and_then(|f| self.dock.find_tab(&f))
            .or_else(|| self.dock.find_tab_from(|t| matches!(t, Tab::Doc(_))));
        match near {
            Some(path) => {
                self.dock.set_focused_node_and_surface(path.node_path());
                self.dock.push_to_focused_leaf(tab);
            }
            None => match self.dock.find_tab(&Tab::Browser) {
                Some(browser) => {
                    self.dock
                        .main_surface_mut()
                        .split_right(browser.node, 0.24, vec![tab]);
                }
                None => self.dock.push_to_focused_leaf(tab),
            },
        }
    }

    /// Closes windows; for those with an open transaction it asks first
    /// whether to commit or roll it back.
    fn close_windows(&mut self, ids: Vec<DocId>) {
        let (open, idle): (Vec<DocId>, Vec<DocId>) =
            ids.into_iter().partition(|id| self.has_pending_tx(*id));
        for id in idle {
            self.remove_window(id);
        }
        if open.is_empty() {
            return;
        }
        let names: Vec<String> = open
            .iter()
            .filter_map(|id| self.docs.get(id))
            .map(Document::title)
            .collect();
        let text = match names.as_slice() {
            [one] => format!("{one} has uncommitted changes."),
            many => format!("{} have uncommitted changes.", many.join(", ")),
        };
        if let Some(&first) = open.first() {
            self.open_tab(Tab::Doc(first));
        }
        self.confirm = Some(Confirm {
            action: ConfirmAction::CloseWindows(open),
            title: "Uncommitted transaction".into(),
            text,
            yes: "Commit and close",
            other: Some("Roll back and close"),
        });
    }

    /// Takes a window's tab out of the dock and closes it.
    fn remove_window(&mut self, id: DocId) {
        if let Some(path) = self.dock.find_tab(&Tab::Doc(id)) {
            self.dock.remove_tab(path);
        }
        self.close_doc(id);
    }

    fn close_doc(&mut self, id: DocId) {
        self.conns.close_doc(id);
        if let Some(doc) = self.docs.remove(&id) {
            self.closed_docs.push(doc);
            if self.closed_docs.len() > 20 {
                self.closed_docs.remove(0);
            }
        }
        self.routes.retain(|_, d| *d != id);
        if self.focused == Some(id) {
            // Follow the tab the dock shows now, like the next frame would.
            self.focused = match self.dock.find_active_focused() {
                Some((_, Tab::Doc(doc))) if self.docs.contains_key(doc) => Some(*doc),
                _ => self.docs.keys().next().copied(),
            };
        }
    }

    /// Carries out what a document (or the browser, `doc == None`) asked for.
    fn apply(&mut self, doc: Option<DocId>, effects: Effects) {
        let connection = doc
            .and_then(|id| self.docs.get(&id))
            .and_then(|d| d.connection.clone());
        let close = effects.close;
        for (target, request) in effects.requests {
            let target_connection = match &target {
                Target::Connection(c) => Some(c.clone()),
                _ => connection.clone(),
            };
            let Some(name) = target_connection.filter(|c| self.conns.get(c).is_some()) else {
                if let Some(id) = doc {
                    let event = DbEvent::Error(pythia_db::DbError::new(
                        "Not connected. Connect first (Session → Connect…) or pick a connection for this window.",
                    ));
                    self.deliver(id, event);
                }
                continue;
            };
            match (target, doc) {
                (Target::Own | Target::Connection(_), Some(id)) => {
                    let autocommit = self
                        .conns
                        .get(&name)
                        .is_some_and(|c| self.autocommit_for(id, c));
                    let conn = self.conns.get_mut(&name).expect("checked above");
                    conn.doc_session(id, autocommit, &self.notify)
                        .session
                        .send(request);
                }
                (Target::Meta, doc) if let Some(req) = request_key(&request) => {
                    if let Some(id) = doc {
                        self.routes.insert(req, id);
                    }
                    let conn = self.conns.get_mut(&name).expect("checked above");
                    conn.send_meta(req, request, &self.notify);
                }
                (_, doc) => {
                    if let (Some(id), Some(req)) = (doc, request_key(&request)) {
                        self.routes.insert(req, id);
                    }
                    self.conns.send_main(&name, request);
                }
            }
        }
        if effects.cancel
            && let (Some(id), Some(name)) = (doc, &connection)
            && let Some(s) = self.conns.get(name).and_then(|c| c.docs.get(&id))
        {
            s.session.cancel();
        }
        if let Some(name) = &connection {
            let (mut ddl, mut plsql) = (false, false);
            for sql in effects.executed {
                let first = sql
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                ddl |= matches!(
                    first.as_str(),
                    "CREATE" | "ALTER" | "DROP" | "RENAME" | "COMMENT" | "TRUNCATE"
                );
                // Blocks and calls compile and drop objects too
                // (DBMS_UTILITY.COMPILE_SCHEMA, EXECUTE IMMEDIATE, …).
                plsql |= matches!(
                    first.as_str(),
                    "BEGIN" | "DECLARE" | "CALL" | "EXEC" | "EXECUTE"
                );
                self.history.add(name, &sql);
            }
            if ddl {
                self.invalidate_catalog(name);
            }
            if ddl || plsql {
                self.refresh_invalid(name);
            }
        }
        for line in effects.log {
            self.log(line);
        }
        if let Some(name) = &connection {
            self.fetch_catalog(name, effects.catalog_requests);
        }
        for (unit, line) in effects.breakpoints {
            self.debug.toggle_breakpoint(unit, line);
        }
        for (connection, action) in effects.object_actions {
            self.object_action(connection, action);
        }
        if let (Some(request), Some(id), Some(name)) = (effects.recompile, doc, &connection)
            && let Some(conn) = self.conns.get_mut(name)
        {
            let params = conn.params().clone();
            let task_id = request.id;
            let task = crate::invalid_window::recompile_task(request, params);
            conn.doc_session(id, false, &self.notify)
                .session
                .send(DbRequest::Task { id: task_id, task });
        }
        // An editor's DDL makes dependents invalid or valid too.
        if (effects.invalid_changed || effects.changed.is_some())
            && let Some(name) = &connection
        {
            self.refresh_invalid(name);
        }
        if let (Some(id), Some((part, offset))) = (doc, effects.jumped_from) {
            self.nav.record(Position {
                doc: id,
                part,
                offset,
            });
        }
        if let (Some(id), Some(references)) = (doc, effects.references) {
            if references.is_empty() {
                self.log("No references found");
            } else {
                self.reference_list.show(id, references);
            }
        }
        self.queued_commands.extend(effects.commands);
        if let (Some(block), Some(id)) = (effects.debug, doc) {
            self.start_debug(id, block);
        }
        for request in effects.ai {
            let source = doc
                .and_then(|id| self.docs.get(&id))
                .map(Document::title)
                .unwrap_or_default();
            self.ai_start(request, None, source, connection.clone());
        }
        // A job form applied: the connection's Scheduler Jobs windows list
        // the job as it is now.
        if let (Some(object), Some(name)) = (&effects.changed, &connection)
            && object.object_type == pythia_db::ObjectType::Job
        {
            let windows: Vec<DocId> = self
                .docs
                .iter()
                .filter(|(_, d)| {
                    d.connection.as_deref() == Some(name.as_str())
                        && matches!(d.kind, DocKind::Jobs(_))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in windows {
                if let Some(DocKind::Jobs(j)) = self.docs.get_mut(&id).map(|d| &mut d.kind) {
                    let effects = j.refresh();
                    self.apply(Some(id), effects);
                }
            }
        }
        if let (Some(object), Some(name)) = (effects.changed, &connection)
            && self.browser.connection.as_deref() == Some(name.as_str())
        {
            let effects = self.browser.show_changed(object);
            self.apply_browser(effects);
        }
        for request in effects.open {
            self.open(request);
        }
        if close && let Some(id) = doc {
            if let Some(path) = self.dock.find_tab(&Tab::Doc(id)) {
                self.dock.remove_tab(path);
            }
            self.close_doc(id);
        }
    }

    /// Learns formatter settings from the focused editor, or (in the
    /// background) from the current schema's source.
    fn learn_format(&mut self, from: crate::settings_dialog::LearnFrom) {
        use crate::settings_dialog::LearnFrom;
        let base = self.settings.format.clone();
        match from {
            LearnFrom::Editor => {
                let doc = self.focused.and_then(|id| self.docs.get(&id));
                let text = match doc.map(|d| &d.kind) {
                    Some(DocKind::Sql(w)) => Some(w.editor.text().to_owned()),
                    Some(DocKind::Program(w)) => Some(
                        w.parts
                            .iter()
                            .map(|p| p.editor.text())
                            .collect::<Vec<_>>()
                            .join("\n/\n"),
                    ),
                    _ => None,
                };
                let from = doc.map_or("the focused window".into(), |d| d.title());
                let learned = text
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| pythia_plsql::infer_format(&t, &base));
                self.settings_dialog.learned(from, learned);
            }
            LearnFrom::Schema => {
                let Some(connection) = self.current_connection() else {
                    self.settings_dialog
                        .learned("a schema (not connected)".into(), None);
                    return;
                };
                let schema = self
                    .conns
                    .get(&connection)
                    .map(|c| c.current_schema())
                    .unwrap_or_default();
                let id = request_id();
                self.learn_inflight = Some(id);
                let task: pythia_db::TaskFn = Box::new(move |c: &pythia_db::oracle::Connection| {
                    let rows = c.query_as::<(String, String, Option<String>)>(
                        &pythia_db::compat::first_rows(
                            "select name, type, text from all_source where owner = :1 \
                             and type in ('PACKAGE', 'PACKAGE BODY', 'PROCEDURE', 'FUNCTION', \
                             'TRIGGER', 'TYPE BODY') order by name, type, line",
                            50000,
                        ),
                        &[&schema],
                    )?;
                    let mut src = String::new();
                    let mut unit = None;
                    for row in rows {
                        let (name, kind, text) = row?;
                        if unit.as_ref() != Some(&(name.clone(), kind.clone())) {
                            if unit.is_some() {
                                src.push_str("\n/\n");
                            }
                            unit = Some((name, kind));
                        }
                        src.push_str(&text.unwrap_or_default());
                    }
                    let learned =
                        (!src.trim().is_empty()).then(|| pythia_plsql::infer_format(&src, &base));
                    Ok(Box::new((format!("schema {schema}"), learned))
                        as Box<dyn std::any::Any + Send>)
                });
                self.conns
                    .send_main(&connection, DbRequest::Task { id, task });
            }
        }
    }

    /// Counts the connection's invalid objects in the background (on the main
    /// session: a cheap query), for the status bar.
    fn refresh_invalid(&mut self, connection: &str) {
        let Some(conn) = self
            .conns
            .get(connection)
            .filter(|c| c.is_connected() && c.dialect().supports(Feature::InvalidObjects))
        else {
            return;
        };
        if self.invalid_inflight.values().any(|c| c == connection) {
            // It may have counted before the change.
            self.invalid_again.insert(connection.to_owned());
            return;
        }
        let scope = match self.settings.invalid.scope {
            pythia_config::InvalidScope::CurrentSchema => {
                pythia_db::recompile::InvalidScope::Schema(conn.current_schema())
            }
            pythia_config::InvalidScope::UserSchemas => {
                pythia_db::recompile::InvalidScope::UserSchemas
            }
        };
        let id = request_id();
        self.invalid_inflight.insert(id, connection.to_owned());
        let task: pythia_db::TaskFn = Box::new(move |c: &pythia_db::oracle::Connection| {
            Ok(Box::new(pythia_db::recompile::invalid_counts(c, &scope)?)
                as Box<dyn std::any::Any + Send>)
        });
        self.conns
            .send_main(connection, DbRequest::Task { id, task });
    }

    /// Refreshes the counts on the interval from the settings.
    fn refresh_invalid_periodically(&mut self, ctx: &egui::Context) {
        let minutes = self.settings.invalid.refresh_minutes;
        if minutes == 0 {
            return;
        }
        let every = std::time::Duration::from_secs(u64::from(minutes) * 60);
        let due: Vec<String> = self
            .conns
            .open
            .iter()
            .filter(|(_, c)| c.is_connected())
            .map(|(name, _)| name.clone())
            .filter(|name| {
                self.invalid
                    .get(name)
                    .is_none_or(|c| c.at.elapsed() >= every)
            })
            .collect();
        for name in due {
            self.refresh_invalid(&name);
        }
        ctx.request_repaint_after(every);
    }

    /// Remembers the focused editor's place before a jump.
    fn record_jump(&mut self) {
        if let Some(position) = self.current_position() {
            self.nav.record(position);
        }
    }

    fn current_position(&self) -> Option<Position> {
        let doc = self.focused?;
        let (part, offset) = self.docs.get(&doc)?.position()?;
        Some(Position { doc, part, offset })
    }

    /// Back (or forward) to where a jump started, skipping closed windows.
    fn navigate_history(&mut self, back: bool) {
        loop {
            let target = if back {
                self.nav.back.pop()
            } else {
                self.nav.forward.pop()
            };
            let Some(target) = target else {
                return;
            };
            if !self.docs.contains_key(&target.doc) {
                continue;
            }
            if let Some(current) = self.current_position() {
                if back {
                    self.nav.forward.push(current);
                } else {
                    self.nav.back.push(current);
                }
            }
            self.open_tab(Tab::Doc(target.doc));
            if let Some(doc) = self.docs.get_mut(&target.doc) {
                doc.goto_position(target.part, target.offset..target.offset);
            }
            return;
        }
    }

    /// Carries out an object action chosen in an editor with the object
    /// browser, attached to the editor's connection if it shows another.
    fn object_action(&mut self, connection: String, action: crate::browser::ObjectAction) {
        if self.browser.connection.as_deref() != Some(connection.as_str()) {
            // The action needs the browser there now, even if switching the
            // windows asks first.
            self.attach_browser(connection.clone());
            self.activate(connection);
        }
        if matches!(action, crate::browser::ObjectAction::Reveal(_)) {
            self.open_tab(Tab::Browser);
        }
        let ctx = self.notify.0.clone();
        let effects = self.browser.perform(action, &ctx);
        self.apply_browser(effects);
    }

    // ----- AI assistant ---------------------------------------------------

    /// Asks the assistant about the focused window.
    fn ai_ask(&mut self, task: pythia_ai::Task) {
        use pythia_ai::Task;
        let doc = self
            .focused
            .and_then(|id| self.docs.get(&id).map(|d| (id, d)));
        let snapshot = match doc.map(|(id, d)| (id, &d.kind)) {
            Some((id, DocKind::Sql(w))) => crate::ai::context::sql_window(w, id, &task),
            Some((id, DocKind::Program(w))) => crate::ai::context::program_window(w, id, &task),
            _ => Default::default(),
        };
        let connection = doc.and_then(|(_, d)| d.connection.clone());
        let needs_code = matches!(
            task,
            Task::Explain | Task::Fix | Task::Rewrite | Task::AnalyzePlan
        );
        let problem = if needs_code && !snapshot.has_code() {
            Some("Put the cursor on a statement in a SQL or program window, or select code, first")
        } else if task == Task::AnalyzePlan && snapshot.lookup.explain.is_none() {
            Some(
                "Plans are analyzed for SQL statements: put the cursor on one in a SQL window, or select one",
            )
        } else if task == Task::AnalyzePlan
            && !connection
                .as_deref()
                .and_then(|c| self.conns.get(c))
                .is_some_and(|c| c.is_connected())
        {
            Some("Connect first: the plan comes from the database")
        } else {
            None
        };
        if let Some(problem) = problem {
            self.open_tab(Tab::Assistant);
            self.assistant.notice(problem);
            return;
        }
        let request = crate::ai::AiRequest {
            task,
            parts: snapshot.parts,
            lookup: snapshot.lookup,
        };
        self.ai_start(request, snapshot.origin, snapshot.source, connection);
    }

    /// Starts an assistant request: dictionary lookups on the connection
    /// first if there are any, then the model.
    fn ai_start(
        &mut self,
        request: crate::ai::AiRequest,
        origin: Option<crate::ai::Origin>,
        source: String,
        connection: Option<String>,
    ) {
        self.open_tab(Tab::Assistant);
        let Some(backend) = self.assistant.backend(&self.settings.ai).cloned() else {
            // The panel says how to set one up.
            return;
        };
        if self.assistant.is_busy() {
            self.assistant
                .notice("Still answering the last request: wait for it or stop it first");
            return;
        }
        let conn = connection
            .as_deref()
            .and_then(|c| self.conns.get(c))
            .filter(|c| c.is_connected());
        // Without dictionary lookups the model answers from the request alone.
        let lookups = conn.is_some_and(|c| c.dialect().supports(Feature::AiLookups));
        let mut parts = Vec::new();
        if let Some(server) = conn.and_then(|c| c.server()) {
            parts.push(crate::ai::context::connection_part(server));
        }
        parts.extend(request.parts);
        let entry = self
            .assistant
            .add(request.task, parts, origin, source, &backend);
        match connection.filter(|_| lookups) {
            Some(name) if !request.lookup.is_empty() => {
                let id = request_id();
                self.assistant.look_up(entry, id);
                let task = crate::ai::lookup::task(request.lookup);
                self.conns.send_main(&name, DbRequest::Task { id, task });
            }
            _ => self
                .assistant
                .ask(entry, &self.settings.ai, self.notify.callback()),
        }
    }

    fn assistant_actions(&mut self, actions: Vec<crate::ai::PanelAction>) {
        use crate::ai::PanelAction;
        for action in actions {
            match action {
                PanelAction::Submit(task) => self.ai_ask(task),
                PanelAction::Retry(entry) => {
                    self.assistant
                        .ask(entry, &self.settings.ai, self.notify.callback())
                }
                PanelAction::Insert { origin, text } => self.ai_insert(origin, &text),
                PanelAction::Replace {
                    entry,
                    origin,
                    text,
                } => self.ai_replace(entry, origin, &text),
                PanelAction::NewSqlWindow(text) => match self.current_connection() {
                    Some(connection) => self.open(OpenRequest::Sql {
                        connection,
                        text,
                        run: false,
                    }),
                    None => {
                        let title = self.new_sql_title();
                        self.add_doc(None, DocKind::Sql(SqlWindow::new(title, text)));
                    }
                },
                PanelAction::OpenSettings => {
                    self.settings_dialog.open = true;
                    self.settings_dialog.show_ai();
                }
            }
        }
    }

    /// The editor of a SQL window, or part `part` of a program window
    /// (made the active one).
    fn editor_mut(&mut self, doc: DocId, part: usize) -> Option<&mut crate::editor::Editor> {
        match &mut self.docs.get_mut(&doc)?.kind {
            DocKind::Sql(w) => Some(&mut w.editor),
            DocKind::Program(w) if part < w.parts.len() => {
                w.active = part;
                Some(&mut w.parts[part].editor)
            }
            _ => None,
        }
    }

    /// Formats the focused editor, then asks the AI to lay out each
    /// statement the formatter keeps as written (unsupported constructs,
    /// failed safety checks), by the same rules.
    fn ai_format(&mut self) {
        let Some(doc) = self.focused else {
            return;
        };
        let part = match self.docs.get(&doc).map(|d| &d.kind) {
            Some(DocKind::Program(w)) => w.active,
            _ => 0,
        };
        if self.ai_format.is_some() {
            self.log("Format with AI: already running");
            return;
        }
        let Some(backend) = self.assistant.backend(&self.settings.ai).cloned() else {
            self.log("Format with AI: set up a model in Settings → AI first");
            return;
        };
        let options = self.settings.format.clone();
        let Some(editor) = self.editor_mut(doc, part) else {
            self.log("Format with AI: put the cursor in a SQL or program window");
            return;
        };
        editor.format_all(&options);
        let text = editor.text().to_owned();
        let kept = pythia_plsql::kept_as_written(&text, &options);
        if kept.is_empty() {
            self.log("Format with AI: the formatter handles every statement here");
            return;
        }
        let ai = &self.settings.ai;
        let rules = pythia_plsql::describe_format(&options);
        let timeout = std::time::Duration::from_secs(ai.timeout_secs.max(5));
        let mut jobs = Vec::new();
        for range in kept {
            let original = text[range.clone()].to_owned();
            let turn = pythia_ai::Turn::new(
                pythia_ai::Task::Format(rules.clone()),
                vec![pythia_ai::Part::code("Statement", original.clone())],
            );
            let (request, _) = pythia_ai::prompt::request(
                &[turn],
                pythia_ai::Policy::new(ai),
                backend.context_chars.max(2000),
            );
            let job = crate::ai::spawn(
                backend.clone(),
                request,
                timeout,
                self.notify.callback(),
                self.assistant.responder.clone(),
            );
            jobs.push(AiFormatJob {
                range,
                original,
                job,
                result: None,
            });
        }
        self.log(format!(
            "Format with AI: asking {} to lay out {} statement(s) the formatter keeps as written",
            backend.name,
            jobs.len()
        ));
        self.ai_format = Some(AiFormatRun { doc, part, jobs });
    }

    /// Applies the AI's layouts once every answer is in: each only if it
    /// changes nothing but whitespace and case, and the statement wasn't
    /// edited meanwhile; all in one undo step.
    fn poll_ai_format(&mut self) {
        let Some(run) = &mut self.ai_format else {
            return;
        };
        for j in &mut run.jobs {
            if j.result.is_none() {
                j.result = j.job.poll();
            }
        }
        if run.jobs.iter().any(|j| j.result.is_none()) {
            return;
        }
        let Some(run) = self.ai_format.take() else {
            return;
        };
        let Some(editor) = self.editor_mut(run.doc, run.part) else {
            self.log("Format with AI: the window was closed");
            return;
        };
        let mut text = editor.text().to_owned();
        let total = run.jobs.len();
        let mut laid_out = 0;
        // Where kept statements start, and why they were kept.
        let mut kept = Vec::new();
        // Replacements made: where, and how much longer the text got.
        let mut grown: Vec<(usize, isize)> = Vec::new();
        // From the last statement back, so the earlier ranges stay valid.
        for j in run.jobs.into_iter().rev() {
            let code = j
                .result
                .unwrap_or_else(|| Err("no answer".into()))
                .and_then(|answer| {
                    pythia_ai::answer::blocks(&answer)
                        .into_iter()
                        .find_map(|b| match b {
                            pythia_ai::answer::Block::Code { code, .. } => {
                                Some(crate::ai::fit_terminator(&j.original, code))
                            }
                            pythia_ai::answer::Block::Text(_) => None,
                        })
                        .ok_or_else(|| "the answer had no code".to_owned())
                });
            let problem = match code {
                Err(e) => e,
                Ok(_) if text.get(j.range.clone()) != Some(j.original.as_str()) => {
                    "it was edited meanwhile".into()
                }
                Ok(code) if !pythia_plsql::only_layout_differs(&j.original, &code) => {
                    "the answer changed more than the layout".into()
                }
                Ok(code) => {
                    grown.push((j.range.start, code.len() as isize - j.range.len() as isize));
                    text.replace_range(j.range, &code);
                    laid_out += 1;
                    continue;
                }
            };
            kept.push((j.range.start, problem));
        }
        editor.apply_format(&text);
        let mut message = format!("Format with AI: laid out {laid_out} of {total} statement(s)");
        if !kept.is_empty() {
            let lines: Vec<String> = kept
                .into_iter()
                .rev()
                .map(|(start, problem)| {
                    let shift: isize = grown
                        .iter()
                        .filter(|(at, _)| *at < start)
                        .map(|(_, d)| d)
                        .sum();
                    let at = start.saturating_add_signed(shift).min(text.len());
                    let line = text[..at].matches('\n').count() + 1;
                    format!("line {line}: {problem}")
                })
                .collect();
            message.push_str(&format!("; kept as written: {}", lines.join("; ")));
        }
        self.log(message);
    }

    /// Inserts code from an answer at the cursor of the editor the request
    /// came from (or the focused one, or a new SQL window).
    fn ai_insert(&mut self, origin: Option<crate::ai::Origin>, text: &str) {
        let target = origin
            .map(|o| (o.doc, o.part))
            .filter(|(doc, _)| self.docs.contains_key(doc))
            .or_else(|| {
                let id = self.focused?;
                match &self.docs.get(&id)?.kind {
                    DocKind::Program(w) => Some((id, w.active)),
                    DocKind::Sql(_) => Some((id, 0)),
                    _ => None,
                }
            });
        match target {
            Some((doc, part)) if self.editor_mut(doc, part).is_some() => {
                if let Some(editor) = self.editor_mut(doc, part) {
                    editor.insert(text);
                }
                self.open_tab(Tab::Doc(doc));
            }
            _ => {
                self.assistant_actions(vec![crate::ai::PanelAction::NewSqlWindow(text.to_owned())])
            }
        }
    }

    /// Replaces the statement or selection a request was about with code
    /// from its answer, if it is still unchanged.
    fn ai_replace(&mut self, entry: usize, origin: crate::ai::Origin, text: &str) {
        let Some((range, original)) = origin.target else {
            return;
        };
        let Some(editor) = self.editor_mut(origin.doc, origin.part) else {
            self.assistant
                .notice("Its window is closed: use Copy or New SQL window instead");
            return;
        };
        if editor.text().get(range.clone()) != Some(original.as_str()) {
            self.assistant
                .notice("The code changed since it was asked about: use Insert or Copy instead");
            return;
        }
        let text = crate::ai::fit_terminator(&original, text);
        let line = editor.text()[..range.start].matches('\n').count() + 1;
        editor.replace(range.clone(), &text, line);
        let replaced = range.start..range.start + text.len();
        editor.select(replaced.clone());
        self.assistant.replaced(entry, replaced, text);
        self.open_tab(Tab::Doc(origin.doc));
    }

    /// Starts a debug run of `block` on the document's session.
    fn start_debug(&mut self, doc: DocId, block: String) {
        if self.debug.is_active() {
            self.log("A debug session is already running");
            return;
        }
        // The block could change anything (#115).
        if self.read_only_of(self.docs.get(&doc).and_then(|d| d.connection.as_deref())) {
            if let Some(DocKind::Sql(w)) = self.docs.get_mut(&doc).map(|d| &mut d.kind) {
                w.state.debug_start = None;
                w.refuse_read_only();
            }
            return;
        }
        let host = Some(self.settings.debug_host.trim().to_owned())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(pythia_debugger::target::default_host);
        let ports = match pythia_debugger::setup::parse_ports(&self.settings.debug_ports) {
            Ok(ports) => ports,
            Err(e) => {
                self.log(format!(
                    "Debugger ports (Settings → Debugger): {e}; using any port"
                ));
                None
            }
        };
        let entry = match self.docs.get(&doc).map(|d| &d.kind) {
            Some(DocKind::Sql(w)) => w.state.debug_entry.clone(),
            _ => None,
        };
        self.debug
            .set_plan_statistics(self.settings.debug_plan_statistics);
        self.debug
            .set_open_sql_on_flag(self.settings.debug_open_sql_panel);
        let actions = self
            .debug
            .start(block, host, ports.as_ref(), entry, self.notify.callback());
        self.debug_run = Some((doc, 0));
        self.debug_connection = self.docs.get(&doc).and_then(|d| d.connection.clone());
        // Only this window shows where its block is stopped.
        for (id, d) in &mut self.docs {
            if let DocKind::Sql(w) = &mut d.kind
                && *id != doc
            {
                w.state.debug_start = None;
            }
        }
        self.open_debugger();
        self.open_tab(Tab::Doc(doc));
        self.debug_actions(actions);
    }

    /// Opens the debugger setup for the connection of the last debug run,
    /// else the current one. `reason` is why: a run that didn't connect.
    fn open_debug_setup(&mut self, reason: Option<String>) {
        let connected = |c: &String| self.conns.get(c).is_some_and(Connection::is_connected);
        let Some(name) = self
            .debug_connection
            .clone()
            .filter(connected)
            .or_else(|| self.current_connection().filter(connected))
        else {
            self.log("Debugger setup: connect to an Oracle database first");
            return;
        };
        let conn = &self.conns.open[&name];
        if !conn.dialect().supports(Feature::Debugger) {
            self.log(format!("Debugger setup: {name} has no PL/SQL debugger"));
            return;
        }
        let params = conn.params().clone();
        self.debug_setup
            .open(&self.notify.0, name, params, &self.settings, reason);
    }

    /// Continues the stopped debug run to the cursor line of the focused
    /// program window, or of the block being debugged in a SQL window.
    fn debug_run_to_cursor(&mut self) {
        let target = match self
            .focused
            .and_then(|id| self.docs.get(&id))
            .map(|d| &d.kind)
        {
            Some(DocKind::Program(w)) => w.cursor_line(),
            Some(DocKind::Sql(w)) => w.state.debug_start.and_then(|start| {
                // The block is the outermost frame.
                let block = self.debug.stack.last()?.location.unit.clone();
                let line = w.editor.line_col().0;
                (block.unit_type == pythia_debugger::UnitType::Block && line >= start)
                    .then(|| (block, (line - start + 1) as u32))
            }),
            _ => None,
        };
        match target {
            Some((unit, line)) => self.debug.run_to(unit, line),
            None => self.log(
                "Run to cursor: put the cursor in a program window, or in the block being debugged",
            ),
        }
    }

    fn debug_actions(&mut self, actions: Vec<pythia_debugger::ui::DebugAction>) {
        use pythia_debugger::ui::DebugAction;
        for action in actions {
            match action {
                DebugAction::RunTarget {
                    host,
                    port,
                    block,
                    statistics,
                } => {
                    let Some((doc, _)) = self.debug_run else {
                        continue;
                    };
                    let Some(connection) = self.docs.get(&doc).and_then(|d| d.connection.clone())
                    else {
                        continue;
                    };
                    let autocommit = self
                        .conns
                        .get(&connection)
                        .is_some_and(|c| self.autocommit_for(doc, c));
                    let Some(conn) = self.conns.get_mut(&connection) else {
                        continue;
                    };
                    let session = conn.doc_session(doc, autocommit, &self.notify);
                    // Units with breakpoints, and the one to stop on entry
                    // to, need debug info; compile those without on the
                    // same session so it happens before the block runs.
                    let units: std::collections::BTreeSet<pythia_debugger::Unit> = self
                        .debug
                        .breakpoints
                        .iter()
                        .map(|b| b.unit.clone())
                        .chain(self.debug.entry.iter().map(|(unit, _)| unit.clone()))
                        .collect();
                    for unit in units {
                        let task: pythia_db::TaskFn =
                            Box::new(move |c: &pythia_db::oracle::Connection| {
                                pythia_debugger::target::ensure_debug_info(c, &unit)?;
                                Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                            });
                        session.session.send(DbRequest::Task {
                            id: request_id(),
                            task,
                        });
                    }
                    // Who the session is, for the SQL panel's lookups.
                    let id = request_id();
                    session.session.send(DbRequest::Task {
                        id,
                        task: pythia_debugger::target::session_task(),
                    });
                    self.debug_session_task = Some(id);
                    let id = request_id();
                    session.session.send(DbRequest::Task {
                        id,
                        task: pythia_debugger::target::debug_task(host, port, block, statistics),
                    });
                    self.debug_run = Some((doc, id));
                }
                DebugAction::ShowLocation(location) => {
                    use pythia_debugger::UnitType;
                    let object_type = match location.unit.unit_type {
                        UnitType::Procedure => Some(pythia_db::ObjectType::Procedure),
                        UnitType::Function => Some(pythia_db::ObjectType::Function),
                        UnitType::Package => Some(pythia_db::ObjectType::Package),
                        UnitType::PackageBody => Some(pythia_db::ObjectType::PackageBody),
                        UnitType::Trigger => Some(pythia_db::ObjectType::Trigger),
                        UnitType::Type => Some(pythia_db::ObjectType::Type),
                        UnitType::TypeBody => Some(pythia_db::ObjectType::TypeBody),
                        UnitType::Block => {
                            // The SQL window marks the line itself.
                            if let Some((doc, _)) = self.debug_run {
                                self.open_tab(Tab::Doc(doc));
                            }
                            None
                        }
                    };
                    // Also once the run ended (the SQL panel's line links).
                    let connection = self
                        .debug_run
                        .and_then(|(doc, _)| self.docs.get(&doc))
                        .and_then(|d| d.connection.clone())
                        .or_else(|| self.debug_connection.clone());
                    if let (Some(object_type), Some(connection), Some(line)) =
                        (object_type, connection, location.line)
                    {
                        let object = ObjectRef {
                            owner: location.unit.owner,
                            name: location.unit.name,
                            object_type,
                        };
                        self.open(OpenRequest::ProgramAt {
                            connection,
                            object,
                            line: line as usize,
                        });
                    }
                }
                DebugAction::ExplainOpaque {
                    unit,
                    line,
                    names,
                    source,
                } => {
                    let Some(connection) = self
                        .debug_run
                        .and_then(|(doc, _)| self.docs.get(&doc))
                        .and_then(|d| d.connection.clone())
                    else {
                        continue;
                    };
                    let Some(conn) = self.conns.get_mut(&connection) else {
                        continue;
                    };
                    let id = request_id();
                    self.opaque_inflight.insert(id, unit.clone());
                    let task: pythia_db::TaskFn =
                        Box::new(move |c: &pythia_db::oracle::Connection| {
                            let reasons = pythia_debugger::target::opaque_reasons(
                                c,
                                &unit,
                                line,
                                &names,
                                source.as_deref(),
                            )?;
                            Ok(Box::new(reasons) as Box<dyn std::any::Any + Send>)
                        });
                    conn.send_meta(id, DbRequest::Task { id, task }, &self.notify);
                }
                DebugAction::Finished => self.log("Debug session ended"),
                DebugAction::Setup { reason } => self.open_debug_setup(reason),
                DebugAction::FindSql { session, stop } => {
                    let task: pythia_db::TaskFn =
                        Box::new(move |c: &pythia_db::oracle::Connection| {
                            let found = pythia_debugger::sql::session_sql(c, &session)
                                .map_err(|e| pythia_debugger::sql::explain_error(&e));
                            Ok(Box::new(found) as Box<dyn std::any::Any + Send>)
                        });
                    self.debug_meta_task(DebugSqlLookup::Find(stop), task);
                }
                DebugAction::LoadPlan { sql_id, child } => {
                    let key = (sql_id.clone(), child);
                    let task: pythia_db::TaskFn =
                        Box::new(move |c: &pythia_db::oracle::Connection| {
                            let plan = pythia_debugger::sql::cursor_plan(c, &sql_id, child)
                                .map_err(|e| pythia_debugger::sql::explain_error(&e));
                            Ok(Box::new(plan) as Box<dyn std::any::Any + Send>)
                        });
                    self.debug_meta_task(DebugSqlLookup::Plan(key.0, key.1), task);
                }
                DebugAction::OpenSource(location) => {
                    use pythia_debugger::UnitType;
                    let Some(line) = location.line else {
                        continue;
                    };
                    if location.unit.unit_type == UnitType::Block {
                        let Some(doc) = self.debug_doc() else {
                            continue;
                        };
                        if let Some(DocKind::Sql(w)) = self.docs.get_mut(&doc).map(|d| &mut d.kind)
                            && let Some(start) = w.state.debug_start
                        {
                            w.editor.goto(start + line as usize - 1, 1);
                        }
                        self.open_tab(Tab::Doc(doc));
                    } else {
                        self.debug_actions(vec![DebugAction::ShowLocation(location)]);
                    }
                }
                DebugAction::AnalyzeSql(question) => self.debug_analyze_sql(question),
            }
        }
    }

    /// The SQL window of the current or last debug run, whose block the
    /// debugger's anonymous-block lines count in.
    fn debug_doc(&self) -> Option<DocId> {
        self.debug_run.map(|(doc, _)| doc).or_else(|| {
            self.docs.iter().find_map(|(id, d)| match &d.kind {
                DocKind::Sql(w) if w.state.debug_start.is_some() => Some(*id),
                _ => None,
            })
        })
    }

    /// Runs a lookup of the debugger's SQL panel on the debugged
    /// connection's metadata session (the debugged one is busy).
    fn debug_meta_task(&mut self, lookup: DebugSqlLookup, task: pythia_db::TaskFn) {
        let Some(connection) = self.debug_connection.clone() else {
            return;
        };
        let Some(conn) = self.conns.get_mut(&connection) else {
            return;
        };
        let id = request_id();
        self.debug_sql_inflight.insert(id, lookup);
        conn.send_meta(id, DbRequest::Task { id, task }, &self.notify);
    }

    /// Asks the assistant about a statement the debugged session ran: its
    /// plan with the statistics, the code around it and the variables at
    /// the stop (bind values, for the data policy).
    fn debug_analyze_sql(&mut self, question: pythia_debugger::ui::SqlQuestion) {
        use pythia_ai::{Part, Task};
        let mut parts = vec![Part::code("Statement", question.sql.clone())];
        if let Some((title, source)) = question
            .location
            .as_ref()
            .and_then(|l| self.debug_source(l))
        {
            parts.push(Part::code(title, source));
        }
        parts.push(Part::text(
            if question.measured {
                "Execution plan with the last execution's row source statistics \
                 (DBMS_XPLAN ALLSTATS LAST; E-Rows is per start, A-Rows over all starts)"
            } else {
                "Execution plan from the cursor cache (no row source statistics)"
            },
            question.plan,
        ));
        if !question.variables.is_empty() {
            parts.push(Part::bind_values(
                "PL/SQL variables at the stop (the statement's binds :B1… are among them)",
                question.variables.join("\n"),
            ));
        }
        let request = crate::ai::AiRequest {
            task: Task::AnalyzeSlow(question.title.clone()),
            parts,
            lookup: crate::ai::Lookup {
                tables: crate::ai::context::table_refs(&question.sql),
                ..Default::default()
            },
        };
        let connection = self.debug_connection.clone();
        self.ai_start(
            request,
            None,
            format!("Debugger · {}", question.title),
            connection,
        );
    }

    /// The code around a line the debugger names, from the program window
    /// showing the unit or the SQL window with the block being debugged:
    /// (title, numbered lines).
    fn debug_source(&self, location: &pythia_debugger::Location) -> Option<(String, String)> {
        const AROUND: usize = 12;
        let line = location.line? as usize;
        let (text, first) = if location.unit.unit_type == pythia_debugger::UnitType::Block {
            let doc = self.debug_doc()?;
            match self.docs.get(&doc).map(|d| &d.kind) {
                Some(DocKind::Sql(w)) => (w.editor.text(), w.state.debug_start?),
                _ => return None,
            }
        } else {
            self.docs.values().find_map(|d| match &d.kind {
                DocKind::Program(w) => w
                    .parts
                    .iter()
                    .find(|p| {
                        crate::program_window::debug_unit(&w.object, p.object_type) == location.unit
                    })
                    .map(|p| (p.editor.text(), 1)),
                _ => None,
            })?
        };
        let lines: Vec<&str> = text.lines().skip(first - 1).collect();
        let from = line.saturating_sub(AROUND + 1);
        let to = (line + AROUND).min(lines.len());
        if from >= to {
            return None;
        }
        let numbered: Vec<String> = (from..to)
            .map(|i| {
                let mark = if i + 1 == line { ">" } else { " " };
                format!("{mark}{:>5}  {}", i + 1, lines[i])
            })
            .collect();
        Some((
            format!("PL/SQL around line {line} of {}", location.unit),
            numbered.join("\n"),
        ))
    }

    /// Fetches dictionary data for code assistance in the background.
    fn fetch_catalog(&mut self, connection: &str, requests: Vec<pythia_lsp_core::CatalogRequest>) {
        let dialect = self.dialect_of(Some(connection));
        if !dialect.supports(Feature::Completion) {
            return;
        }
        for request in requests {
            if !self
                .catalog_requested
                .insert((connection.to_owned(), request.clone()))
            {
                continue;
            }
            let id = request_id();
            self.catalog_inflight.insert(id, connection.to_owned());
            self.conns
                .send_main(connection, crate::catalog::request(id, request, dialect));
        }
    }

    /// Forgets cached dictionary data (after DDL, or on demand).
    fn invalidate_catalog(&mut self, connection: &str) {
        if let Some(cache) = self.catalogs.get_mut(connection) {
            cache.clear();
        }
        self.catalog_requested.retain(|(c, _)| c != connection);
        self.catalog_revision += 1;
    }

    fn project_actions(&mut self, actions: Vec<crate::project::ProjectAction>) {
        use crate::project::ProjectAction;
        for action in actions {
            match action {
                ProjectAction::Open(path) => {
                    let connection = self.current_connection();
                    self.open_file(path, connection);
                }
                ProjectAction::Diff {
                    title,
                    left,
                    right,
                    file,
                } => {
                    let connection = self.current_connection();
                    let diff = crate::text_diff::TextDiff::new(title, left, right, Some(file));
                    self.add_doc(connection, DocKind::Diff(diff));
                }
                ProjectAction::Deploy(script) => match self.current_connection() {
                    Some(connection) => self.open(OpenRequest::Sql {
                        connection,
                        text: script,
                        run: false,
                    }),
                    None => {
                        self.add_doc(None, DocKind::Sql(SqlWindow::new("Deploy".into(), script)));
                    }
                },
                ProjectAction::Request(connection, request) => {
                    self.conns.send_main(&connection, request);
                }
            }
        }
    }

    /// Opens a file in a SQL window, or shows the window that has it open.
    fn open_file(&mut self, path: std::path::PathBuf, connection: Option<String>) {
        let existing = self.docs.iter().find_map(|(id, d)| match &d.kind {
            DocKind::Sql(w) if w.path.as_deref() == Some(path.as_path()) => Some(*id),
            _ => None,
        });
        if let Some(id) = existing {
            self.open_tab(Tab::Doc(id));
            return;
        }
        match crate::text_file::read(&path) {
            Ok(file) => {
                let connection = connection.or_else(|| self.current_connection());
                self.add_doc(connection, DocKind::Sql(SqlWindow::from_file(path, file)));
            }
            Err(e) => self.log(format!("Could not open {}: {e}", path.display())),
        }
    }

    fn apply_browser(&mut self, effects: Effects) {
        let connection = self.browser.connection.clone();
        let Effects {
            requests,
            open,
            log,
            invalid_changed,
            database,
            ..
        } = effects;
        if let (Some(name), Some(database)) = (&connection, database) {
            self.use_database(name, &database);
        }
        if let Some(name) = connection {
            for (target, request) in requests {
                match (target, request_key(&request), self.conns.get_mut(&name)) {
                    (Target::Meta, Some(id), Some(conn)) => {
                        conn.send_meta(id, request, &self.notify)
                    }
                    _ => {
                        self.conns.send_main(&name, request);
                    }
                }
            }
        }
        for line in log {
            self.log(line);
        }
        for request in open {
            self.open(request);
        }
        if invalid_changed && let Some(name) = self.browser.connection.clone() {
            self.refresh_invalid(&name);
        }
    }

    fn deliver(&mut self, id: DocId, event: DbEvent) {
        let cx = self.doc_context(id);
        let (settings, connected, fetch_size, connection, dialect, read_only, production) = (
            cx.settings.clone(),
            cx.connected,
            cx.fetch_size,
            cx.connection.clone(),
            cx.dialect,
            cx.read_only,
            cx.production,
        );
        let Some(doc) = self.docs.get_mut(&id) else {
            return;
        };
        let catalog = connection.as_ref().and_then(|c| self.catalogs.get(c));
        let cx = DocContext {
            id,
            settings: &settings,
            connected,
            fetch_size,
            connection,
            catalog,
            catalog_revision: self.catalog_revision,
            connections: &[],
            debug: Some(&self.debug),
            marks: None,
            privilege: None,
            dialect,
            read_only,
            production,
        };
        let effects = doc.on_event(event, &cx);
        self.apply(Some(id), effects);
    }

    // ----- commands -------------------------------------------------------

    /// Opens the form (or table builder) for a new object in the schema the
    /// object browser shows, else the connection's current one.
    /// What a maintenance scan on `connection` covers: the schema (Oracle)
    /// or database (MySQL) the object browser shows, else the connection's;
    /// PostgreSQL and SQL Server scan the database (none).
    fn scan_schema(&self, connection: &str) -> String {
        match self.dialect_of(Some(connection)).kind() {
            pythia_db::DbKind::Oracle | pythia_db::DbKind::MySql => self
                .browser
                .schema()
                .filter(|_| self.browser.connection.as_deref() == Some(connection))
                .map(str::to_owned)
                .or_else(|| self.conns.get(connection).map(|c| c.current_schema()))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn new_object(&mut self, object_type: pythia_db::ObjectType) {
        use pythia_db::ObjectType;
        let Some(connection) = self.current_connection() else {
            self.connect_dialog.show(self.last_profile.as_ref());
            return;
        };
        let owner = match object_type {
            t if t.is_database_object() => String::new(),
            _ => self
                .browser
                .schema()
                .filter(|_| self.browser.connection.as_deref() == Some(connection.as_str()))
                // PUBLIC only owns synonyms and links.
                .filter(|s| {
                    *s != "PUBLIC"
                        || matches!(object_type, ObjectType::Synonym | ObjectType::DatabaseLink)
                })
                .map(str::to_owned)
                .or_else(|| self.conns.get(&connection).map(|c| c.current_schema()))
                .unwrap_or_default(),
        };
        self.open(match object_type {
            ObjectType::Table => OpenRequest::NewTable {
                connection,
                owner,
                like: None,
            },
            _ => OpenRequest::EditObject {
                connection,
                target: crate::object_editor::EditTarget::New {
                    owner,
                    object_type,
                    from: None,
                },
            },
        });
    }

    fn run_command(&mut self, ctx: &egui::Context, command: Command) {
        // Shortcuts reach commands the menus don't offer.
        let dialect = self.dialect();
        if !command.available(dialect) {
            self.log(format!(
                "{} isn't available for {} connections",
                command.short_label(),
                dialect.name()
            ));
            return;
        }
        match command {
            Command::NewTable
            | Command::NewView
            | Command::NewMaterializedView
            | Command::NewSequence
            | Command::NewSynonym
            | Command::NewIndex
            | Command::NewTrigger
            | Command::NewProcedure
            | Command::NewFunction
            | Command::NewPackage
            | Command::NewType
            | Command::NewJob
            | Command::NewDatabaseLink
            | Command::NewUser
            | Command::NewRole
            | Command::NewProfile
            | Command::NewDirectory => {
                if let Some((_, object_type)) =
                    Command::NEW_OBJECTS.iter().find(|(c, _)| *c == command)
                {
                    self.new_object(*object_type);
                }
            }
            Command::NewSqlWindow => {
                let title = self.new_sql_title();
                let connection = self.current_connection();
                let mut window = SqlWindow::new(title, String::new());
                // Ready to type into (#157).
                window.editor.request_focus();
                self.add_doc(connection, DocKind::Sql(window));
            }
            Command::CloseTab => {
                if let Some(id) = self.focused
                    && self.dock.find_tab(&Tab::Doc(id)).is_some()
                {
                    self.close_windows(vec![id]);
                }
            }
            Command::CloseOtherTabs => {
                let keep = self.focused;
                let others: Vec<DocId> = self
                    .docs
                    .keys()
                    .copied()
                    .filter(|id| Some(*id) != keep)
                    .collect();
                self.close_windows(others);
            }
            Command::Connect => self.connect_dialog.show(self.last_profile.as_ref()),
            Command::Settings => self.settings_dialog.open = true,
            Command::ReopenClosed => {
                if let Some(doc) = self.closed_docs.pop() {
                    let Document { connection, kind } = doc;
                    self.add_doc(connection, kind);
                }
            }
            Command::DebugStart => {
                if let Some(id) = self.focused {
                    let effects = match self.docs.get_mut(&id).map(|d| &mut d.kind) {
                        Some(DocKind::Sql(w)) => w.debug_current(),
                        _ => {
                            self.log(
                                "Open a SQL window with a PL/SQL block to debug, or right-click a procedure or function → Debug…",
                            );
                            Effects::default()
                        }
                    };
                    self.apply(Some(id), effects);
                }
            }
            Command::ToggleBreakpoint => {
                if let Some(id) = self.focused
                    && let Some(DocKind::Program(w)) = self.docs.get(&id).map(|d| &d.kind)
                {
                    let effects = w.toggle_breakpoint();
                    self.apply(Some(id), effects);
                }
            }
            Command::DebugContinue => self.debug.command(pythia_debugger::DebugCommand::Resume),
            Command::DebugStepOver => self.debug.command(pythia_debugger::DebugCommand::StepOver),
            Command::DebugStepInto => self.debug.command(pythia_debugger::DebugCommand::StepInto),
            Command::DebugStepOut => self.debug.command(pythia_debugger::DebugCommand::StepOut),
            Command::DebugRunToCursor => self.debug_run_to_cursor(),
            Command::DebugStop => self.debug.stop(),
            Command::DebugSetup => self.open_debug_setup(None),
            Command::ShowDebugger => self.open_debugger(),
            Command::ShowVariables => match self.variables_state() {
                // Without variables the panel stays hidden either way:
                // switching it off now would keep it closed later (#168).
                Some((0, _)) => self.log(NO_VARIABLES),
                Some(_) => {
                    if let Some(w) = self.focused_sql_window() {
                        w.show_variables = !w.show_variables;
                    }
                }
                None => self.log(NO_SQL_WINDOW),
            },
            Command::OpenFile => self.file_dialog(FileAction::Open, None),
            Command::SaveFile | Command::SaveFileAs => {
                let Some(id) = self.focused else { return };
                match self.docs.get_mut(&id).map(|d| &mut d.kind) {
                    Some(DocKind::Sql(w)) => match (&w.path, command) {
                        (Some(path), Command::SaveFile) => {
                            let path = path.clone();
                            if self.settings.format_auto.on_save {
                                w.editor.format_all(&self.settings.format);
                            }
                            match w.save_to(path.clone()) {
                                Ok(()) => self.log(format!("Saved {}", path.display())),
                                Err(e) => {
                                    self.log(format!("Could not save {}: {e}", path.display()))
                                }
                            }
                        }
                        _ => {
                            let name = w.path.as_ref().and_then(|p| p.file_name()).map_or_else(
                                || format!("{}.sql", w.title.to_lowercase().replace(' ', "_")),
                                |n| n.to_string_lossy().into_owned(),
                            );
                            self.file_dialog(FileAction::SaveAs(id), Some(name));
                        }
                    },
                    Some(DocKind::Program(w)) => {
                        let name = w.file_name();
                        self.file_dialog(FileAction::SaveAs(id), Some(name));
                    }
                    _ => {}
                }
            }
            Command::Disconnect => {
                let Some(name) = self.current_connection() else {
                    return;
                };
                if self.conns.get(&name).is_some_and(|c| c.any_pending_tx()) {
                    self.confirm = Some(Confirm {
                        action: ConfirmAction::Disconnect(name.clone()),
                        title: "Uncommitted transaction".into(),
                        text: format!(
                            "{name} has uncommitted changes. Disconnecting rolls them back."
                        ),
                        yes: "Disconnect and roll back",
                        other: None,
                    });
                } else {
                    self.disconnect(&name);
                }
            }
            Command::ExecuteAll | Command::ExecuteCurrent => {
                if let Some(id) = self.focused {
                    self.execute(id, command == Command::ExecuteCurrent);
                }
            }
            Command::ExecuteWithStatistics => {
                if let Some(id) = self.focused
                    && let Some(w) = self.docs.get_mut(&id).and_then(Document::sql_window)
                {
                    let effects = w.execute_with_statistics();
                    self.apply(Some(id), effects);
                }
            }
            Command::Profile => {
                if let Some(id) = self.focused {
                    let read_only =
                        self.read_only_of(self.docs.get(&id).and_then(|d| d.connection.as_deref()));
                    let Some(w) = self.docs.get_mut(&id).and_then(Document::sql_window) else {
                        return;
                    };
                    let effects = w.profile(read_only);
                    self.apply(Some(id), effects);
                }
            }
            Command::ExecuteToFile => {
                if let Some(id) = self.focused
                    && let Some(w) = self.docs.get_mut(&id).and_then(Document::sql_window)
                {
                    w.execute_to_file();
                }
            }
            Command::ExplainPlan => {
                if let Some(id) = self.focused
                    && let Some(w) = self.docs.get_mut(&id).and_then(Document::sql_window)
                {
                    let effects = w.explain();
                    self.apply(Some(id), effects);
                }
            }
            Command::Cancel => {
                if let Some(id) = self.focused
                    && let Some(w) = self.docs.get_mut(&id).and_then(Document::sql_window)
                {
                    w.cancel_waiting();
                }
                if let Some((id, name)) = self.focused_session()
                    && let Some(s) = self.conns.get(&name).and_then(|c| c.docs.get(&id))
                {
                    s.session.cancel();
                }
            }
            Command::Commit => {
                let Some((_, name)) = self.focused_session() else {
                    return;
                };
                if self.transaction_state().is_err() {
                    return;
                }
                if self.conns.get(&name).is_some_and(|c| c.profile.production) {
                    self.confirm = Some(Confirm {
                        action: ConfirmAction::Commit,
                        title: "Commit on production".into(),
                        text: format!("Commit the transaction on production connection {name}?"),
                        yes: "Commit",
                        other: None,
                    });
                } else {
                    self.end_transaction(true);
                }
            }
            Command::Rollback => self.end_transaction(false),
            Command::ToggleAutocommit => {
                self.settings.autocommit = !self.settings.autocommit;
                self.save_settings();
                self.sync_autocommit();
                self.log(if self.settings.autocommit {
                    "Auto-commit on for SQL windows (not on production connections)"
                } else {
                    "Auto-commit off"
                });
            }
            Command::History => self.history_dialog.toggle(),
            Command::SaveAsReport => {
                let text = self
                    .focused
                    .and_then(|id| self.docs.get_mut(&id))
                    .and_then(Document::sql_window)
                    .map(|w| {
                        w.statements(true)
                            .into_iter()
                            .next()
                            .map(|s| s.sql)
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    self.log("Put the cursor on a query to save it as a report");
                } else {
                    self.save_report = Some((String::new(), String::new(), text));
                }
            }
            Command::GoToDefinition | Command::PeekDefinition | Command::FindReferences => {
                let request = match command {
                    Command::GoToDefinition => crate::assist::Request::Definition,
                    Command::PeekDefinition => crate::assist::Request::Peek,
                    _ => crate::assist::Request::References,
                };
                if let Some(doc) = self.focused.and_then(|id| self.docs.get_mut(&id)) {
                    doc.navigate(request);
                }
            }
            Command::GoToSymbol => {
                let symbols = self
                    .focused
                    .and_then(|id| self.docs.get(&id))
                    .map(Document::symbols)
                    .unwrap_or_default();
                if symbols.is_empty() {
                    self.log("No declarations to go to here");
                } else {
                    self.symbol_picker.show(symbols);
                }
            }
            Command::SwitchSpecBody => {
                if let Some(id) = self.focused
                    && let Some(DocKind::Program(w)) = self.docs.get_mut(&id).map(|d| &mut d.kind)
                    && let Some((part, offset)) = w.switch_part()
                {
                    self.nav.record(Position {
                        doc: id,
                        part,
                        offset,
                    });
                }
            }
            Command::InvalidObjects => {
                if let Some(connection) = self.current_connection() {
                    let schema = match self.settings.invalid.scope {
                        pythia_config::InvalidScope::CurrentSchema => {
                            self.conns.get(&connection).map(|c| c.current_schema())
                        }
                        pythia_config::InvalidScope::UserSchemas => None,
                    };
                    self.open(OpenRequest::InvalidObjects {
                        connection,
                        schema,
                        recompile: false,
                    });
                } else {
                    self.connect_dialog.show(self.last_profile.as_ref());
                }
            }
            Command::MaintenanceScan => match self.current_connection() {
                Some(connection) => {
                    let schema = self.scan_schema(&connection);
                    self.open(OpenRequest::MaintenanceScan { connection, schema });
                }
                None => self.connect_dialog.show(self.last_profile.as_ref()),
            },
            Command::NavigateBack => self.navigate_history(true),
            Command::NavigateForward => self.navigate_history(false),
            Command::RenameSymbol => {
                let editor = match self
                    .focused
                    .and_then(|id| self.docs.get_mut(&id))
                    .map(|d| &mut d.kind)
                {
                    Some(DocKind::Sql(w)) => Some(&mut w.editor),
                    Some(DocKind::Program(w)) => w.active_editor(),
                    _ => None,
                };
                if let Some(editor) = editor {
                    let message = rename_symbol(editor);
                    if let Some(message) = message {
                        self.log(message);
                    }
                }
            }
            Command::FoldAll | Command::UnfoldAll => {
                let editor = match self
                    .focused
                    .and_then(|id| self.docs.get_mut(&id))
                    .map(|d| &mut d.kind)
                {
                    Some(DocKind::Sql(w)) => Some(&mut w.editor),
                    Some(DocKind::Program(w)) => w.active_editor(),
                    _ => None,
                };
                if let Some(editor) = editor {
                    if command == Command::FoldAll {
                        editor.inner().fold_all();
                    } else {
                        editor.inner().unfold_all();
                    }
                }
            }
            Command::Format => {
                let options = self.settings.format.clone();
                let has_ai = !self.settings.ai.backends.is_empty();
                let editor = match self
                    .focused
                    .and_then(|id| self.docs.get_mut(&id))
                    .map(|d| &mut d.kind)
                {
                    Some(DocKind::Sql(w)) => Some(&mut w.editor),
                    Some(DocKind::Program(w)) => w.active_editor(),
                    _ => None,
                };
                let message = match editor {
                    Some(editor) => {
                        let mut message = format_editor(editor, &options);
                        let kept = pythia_plsql::kept_as_written(editor.text(), &options).len();
                        if kept > 0 {
                            message.push_str(&format!(
                                "; {kept} statement(s) kept as written (unsupported or not safe to change)"
                            ));
                            if has_ai {
                                message.push_str(
                                    ": AI: Format Unsupported Statements can lay them out",
                                );
                            }
                        }
                        message
                    }
                    None => "Nothing to format".into(),
                };
                self.log(message);
            }
            Command::ExportTables => {
                let connection = self.current_connection();
                self.add_doc(connection, DocKind::ExportTables(Default::default()));
            }
            Command::ImportData => {
                let connection = self.current_connection();
                self.add_doc(connection, DocKind::Import(Default::default()));
            }
            Command::SchedulerJobs => {
                let connection = self.current_connection();
                self.add_doc(connection, DocKind::Jobs(Default::default()));
            }
            Command::SearchData => {
                let connection = self.current_connection();
                self.add_doc(connection, DocKind::DataSearch(Default::default()));
            }
            Command::FindInSource => {
                let connection = self.current_connection();
                self.add_doc(
                    connection,
                    DocKind::Search(crate::source_search::SourceSearch::new()),
                );
            }
            Command::CompareSchemas | Command::CompareData => {
                let kind = if command == Command::CompareSchemas {
                    crate::compare_window::CompareKind::Schema
                } else {
                    crate::compare_window::CompareKind::Data
                };
                let connection = self.current_connection();
                self.add_doc(
                    connection.clone(),
                    DocKind::Compare(crate::compare_window::CompareWindow::new(kind, connection)),
                );
            }
            Command::MonitorDashboard
            | Command::MonitorSessions
            | Command::MonitorBlocking
            | Command::MonitorAwr
            | Command::MonitorTrace => {
                use crate::monitor_window::MonitorStart;
                let start = match command {
                    Command::MonitorSessions => MonitorStart::Sessions,
                    Command::MonitorBlocking => MonitorStart::Blocking,
                    Command::MonitorAwr => MonitorStart::Awr,
                    Command::MonitorTrace => MonitorStart::Trace,
                    _ => MonitorStart::Dashboard,
                };
                match self.current_connection() {
                    Some(connection) => self.open(OpenRequest::Monitor { connection, start }),
                    None => self.connect_dialog.show(self.last_profile.as_ref()),
                }
            }
            Command::FindObject => {
                if self.current_connection().is_some() {
                    self.finder.toggle();
                } else {
                    self.connect_dialog.show(self.last_profile.as_ref());
                }
            }
            Command::ShowAssistant | Command::AiAsk | Command::AiGenerateSql => {
                let mode = match command {
                    Command::AiAsk => crate::ai::Mode::Ask,
                    Command::AiGenerateSql => crate::ai::Mode::Generate,
                    _ => self.assistant.mode,
                };
                self.assistant.focus(mode);
                self.open_tab(Tab::Assistant);
            }
            Command::AiExplain => self.ai_ask(pythia_ai::Task::Explain),
            Command::AiFix => self.ai_ask(pythia_ai::Task::Fix),
            Command::AiRewrite => self.ai_ask(pythia_ai::Task::Rewrite),
            Command::AiAnalyzePlan => self.ai_ask(pythia_ai::Task::AnalyzePlan),
            Command::AiFormat => self.ai_format(),
            Command::ShowPalette => self.palette.toggle(),
            Command::ShowBrowser => self.open_tab(Tab::Browser),
            Command::ShowProject => self.open_tab(Tab::Project),
            Command::ShowLog => self.open_tab(Tab::Log),
            Command::ThemeDark | Command::ThemeLight | Command::ThemeSystem => {
                self.settings.theme = match command {
                    Command::ThemeDark => ThemeChoice::Dark,
                    Command::ThemeLight => ThemeChoice::Light,
                    _ => ThemeChoice::System,
                };
                self.apply_settings(ctx);
                self.save_settings();
            }
            Command::ZoomIn | Command::ZoomOut | Command::ZoomReset => {
                self.settings.ui_scale = match command {
                    Command::ZoomIn => (self.settings.ui_scale + 0.1).min(3.0),
                    Command::ZoomOut => (self.settings.ui_scale - 0.1).max(0.5),
                    _ => 1.0,
                };
                self.apply_settings(ctx);
                self.save_settings();
            }
            Command::ResetLayout => self.reset_layout(),
            Command::About => self.about_open = true,
            Command::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    /// The focused document and its connection, if it has one that is open.
    fn focused_session(&self) -> Option<(DocId, String)> {
        let id = self.focused?;
        let name = self.docs.get(&id)?.connection.clone()?;
        self.conns.get(&name).map(|_| (id, name))
    }

    fn execute(&mut self, id: DocId, current_only: bool) {
        let cx = self.doc_context(id);
        let (settings, connected, fetch_size, connection, dialect) = (
            cx.settings.clone(),
            cx.connected,
            cx.fetch_size,
            cx.connection.clone(),
            cx.dialect,
        );
        if !connected {
            if self.docs.get(&id).is_some_and(|d| d.connection.is_none())
                && let Some(name) = self.current_connection()
            {
                // An unbound window runs on the current connection.
                self.docs.get_mut(&id).expect("doc").connection = Some(name);
                return self.execute(id, current_only);
            }
            self.connect_dialog.show(self.last_profile.as_ref());
            return;
        }
        let profile = self
            .docs
            .get(&id)
            .and_then(|d| d.connection.as_deref())
            .and_then(|c| self.conns.get(c))
            .map(|c| c.profile.clone());
        let read_only = profile.as_ref().is_some_and(|p| p.read_only);
        let production = profile.as_ref().is_some_and(|p| p.production);
        // Production: confirm statements that change something.
        if profile.as_ref().is_some_and(|p| p.production)
            && !std::mem::take(&mut self.production_confirmed)
            && let Some(DocKind::Sql(w)) = self.docs.get(&id).map(|d| &d.kind)
        {
            let changing = w
                .statements(current_only)
                .iter()
                .filter(|s| {
                    s.kind != pythia_plsql::StatementKind::SqlPlus
                        && !crate::sql_window::is_query(&s.sql)
                })
                .count();
            if changing > 0 {
                let name = profile.map(|p| p.display_name()).unwrap_or_default();
                self.confirm = Some(Confirm {
                    action: ConfirmAction::Execute {
                        doc: id,
                        current_only,
                    },
                    title: "Change production data?".into(),
                    text: format!(
                        "{changing} statement(s) can modify data or objects on the production connection {name}."
                    ),
                    yes: "Run on production",
                    other: None,
                });
                return;
            }
        }
        let Some(doc) = self.docs.get_mut(&id) else {
            return;
        };
        let catalog = connection.as_ref().and_then(|c| self.catalogs.get(c));
        let cx = DocContext {
            id,
            settings: &settings,
            connected,
            fetch_size,
            connection,
            catalog,
            catalog_revision: self.catalog_revision,
            connections: &[],
            debug: Some(&self.debug),
            marks: None,
            privilege: None,
            dialect,
            read_only,
            production,
        };
        let effects = match &mut doc.kind {
            DocKind::Sql(w) => {
                // The window holds back what a read-only connection can't run.
                w.execute(current_only, &cx)
            }
            DocKind::Program(w) if !read_only => {
                if settings.format_auto.on_save {
                    w.format_before_compile(false, &settings.format);
                }
                w.compile(false, settings.compile_debug_info)
            }
            _ => Effects::default(),
        };
        self.apply(Some(id), effects);
    }

    /// The focused window's own session, if it has one.
    fn focused_doc_session(&self) -> Option<&crate::connections::DocSession> {
        let id = self.focused?;
        let connection = self.docs.get(&id)?.connection.as_deref()?;
        self.conns.get(connection)?.docs.get(&id)
    }

    /// The SQL window that has the focus, if one has.
    fn focused_sql_window(&mut self) -> Option<&mut SqlWindow> {
        let id = self.focused?;
        self.docs.get_mut(&id)?.sql_window()
    }

    /// The focused SQL window's variables: how many, and whether its
    /// Variables panel is switched on. `None` without a SQL window.
    fn variables_state(&mut self) -> Option<(usize, bool)> {
        let w = self.focused_sql_window()?;
        Some((w.variables().len(), w.show_variables))
    }

    /// Whether Commit and Rollback apply to the focused window, or why not.
    fn transaction_state(&self) -> Result<(), &'static str> {
        match self.focused_doc_session() {
            None => Err("No transaction in this window"),
            Some(s) if s.autocommit => Err("Auto-commit is on"),
            Some(s) if !s.pending_tx => Err("No transaction"),
            Some(_) => Ok(()),
        }
    }

    fn has_pending_tx(&self, id: DocId) -> bool {
        self.conns
            .open
            .values()
            .any(|c| c.docs.get(&id).is_some_and(|s| s.pending_tx))
    }

    /// Whether a window's session commits after each statement: SQL windows
    /// with auto-commit on in the settings, except on production
    /// connections, and maintenance windows.
    fn autocommit_for(&self, id: DocId, conn: &Connection) -> bool {
        match self.docs.get(&id).map(|d| &d.kind) {
            Some(DocKind::Sql(_)) => self.settings.autocommit && !conn.profile.production,
            // Its statements commit each (DDL and DBMS_STATS do anyway);
            // VACUUM and REINDEX CONCURRENTLY refuse a transaction block.
            Some(DocKind::Maintenance(_) | DocKind::MaintenanceScan(_)) => true,
            _ => false,
        }
    }

    /// Brings the open window sessions in line with the auto-commit setting.
    fn sync_autocommit(&mut self) {
        let this = &*self;
        let wanted: Vec<(String, DocId, bool)> = this
            .conns
            .open
            .iter()
            .flat_map(|(name, conn)| {
                conn.docs
                    .keys()
                    .map(move |id| (name.clone(), *id, this.autocommit_for(*id, conn)))
            })
            .collect();
        for (name, id, on) in wanted {
            if let Some(s) = self.conns.get_mut(&name).and_then(|c| c.docs.get_mut(&id))
                && s.autocommit != on
            {
                s.autocommit = on;
                s.session.send(DbRequest::SetAutocommit(on));
            }
        }
    }

    /// Whether Debug can start in the focused window, or why not.
    fn debug_state(&self) -> Result<(), &'static str> {
        let doc = self.focused.and_then(|id| self.docs.get(&id));
        let window = match doc.map(|d| &d.kind) {
            Some(DocKind::Sql(w)) => w,
            Some(DocKind::Program(_)) => {
                return Err(
                    "Debugging starts from a SQL window: right-click a subprogram's name → Debug… opens one with a block that calls it",
                );
            }
            _ => return Err("Only in SQL windows"),
        };
        // The block runs on the window's own connection.
        let connected = doc
            .and_then(|d| d.connection.as_deref())
            .and_then(|c| self.conns.get(c))
            .is_some_and(|c| c.is_connected());
        let read_only = self.read_only_of(doc.and_then(|d| d.connection.as_deref()));
        if self.debug.is_active() {
            Err("Already debugging: continue or stop in the debugger panel")
        } else if read_only {
            Err("The connection is read-only")
        } else if doc.is_some_and(Document::is_busy) {
            Err("Running")
        } else if !connected {
            Err("Not connected")
        } else if !window.debuggable() {
            Err("Debugging runs an anonymous block (BEGIN … END;): put the cursor in one")
        } else {
            Ok(())
        }
    }

    /// Commits or rolls back the focused window's session. A window without
    /// a session of its own has nothing to end; none is opened for it.
    fn end_transaction(&mut self, commit: bool) {
        if let Some((id, name)) = self.focused_session()
            && let Some(s) = self.conns.get(&name).and_then(|c| c.docs.get(&id))
        {
            s.session.send(if commit {
                DbRequest::Commit
            } else {
                DbRequest::Rollback
            });
        }
    }

    /// Moves a window to another connection (closing its old session). A
    /// window on an open connection moves only if the settings allow it,
    /// and never with a pending transaction.
    fn rebind(&mut self, id: DocId, connection: String) {
        let on_open = self
            .docs
            .get(&id)
            .and_then(|d| d.connection.as_deref())
            .is_some_and(|c| self.conns.get(c).is_some());
        if on_open && !self.settings.switch_window_connection {
            self.log("Switching a window's connection is off (Settings → Query)");
            return;
        }
        if self.has_pending_tx(id) {
            self.log(
                "Commit or roll back this window's transaction before switching its connection",
            );
            return;
        }
        self.conns.close_doc(id);
        let dialect = self.dialect_of(Some(&connection));
        if let Some(doc) = self.docs.get_mut(&id) {
            doc.connection = Some(connection);
            let effects = doc.on_attach(dialect);
            self.apply(Some(id), effects);
        }
    }

    fn connect(&mut self, profile: ConnectionProfile, passwords: Passwords, save_profile: bool) {
        let name = profile.display_name();
        if self.conns.get(&name).is_some() {
            self.disconnect(&name);
        }
        if save_profile && let Some(paths) = &self.paths {
            self.saved.upsert(profile.clone());
            if let Err(e) = save_toml(&paths.connections_file(), &self.saved) {
                error!("saving connections: {e:#}");
            }
        }
        if profile.save_password {
            secrets::store(&name, &passwords.database);
            secrets::store_ssh(&name, &passwords.ssh);
        } else {
            secrets::forget(&name);
            secrets::forget_ssh(&name);
        }
        match &profile.ssh {
            ssh if ssh.enabled => self.log(format!("Connecting to {name} through {}…", ssh.host)),
            _ => self.log(format!("Connecting to {name}…")),
        }
        self.last_profile = Some(profile.clone());
        self.connecting =
            Some(
                self.conns
                    .connect(profile, passwords, &self.known_hosts, &self.notify),
            );
    }

    fn disconnect(&mut self, name: &str) {
        if self.conns.close(name).is_some() {
            self.log(format!("Disconnected from {name}"));
        }
        self.browser.detach(name);
        for doc in self
            .docs
            .values_mut()
            .filter(|d| d.connection.as_deref() == Some(name))
        {
            if let DocKind::Sql(w) = &mut doc.kind {
                w.state.running = None;
                w.state.fetching = None;
                w.state.queue.clear();
                // Unbound, it runs on the connection the toolbar shows,
                // instead of asking to connect again.
                doc.connection = None;
            }
        }
        // Another open connection becomes the active one; or, without
        // windows on connections of their own, the windows that were on this
        // one follow the active connection.
        if self.browser.connection.is_none()
            && let Some(other) = self.conns.open.keys().next().cloned()
        {
            self.open(OpenRequest::Browse { connection: other });
        } else if !self.settings.switch_window_connection
            && let Some(active) = self.active.clone()
        {
            self.activate(active);
        }
    }

    // ----- database events ------------------------------------------------

    fn pump_events(&mut self) {
        for (name, key, event) in self.conns.poll() {
            let lost = match &event {
                DbEvent::Failed { error, .. } | DbEvent::Error(error) => error.is_connection_lost(),
                DbEvent::Meta {
                    result: Err(error), ..
                }
                | DbEvent::TaskDone {
                    result: Err(error), ..
                } => error.is_connection_lost(),
                _ => false,
            };
            if lost && let Some(conn) = self.conns.get_mut(&name) {
                match key {
                    SessionKey::Doc(id) => {
                        // The next statement in this window opens a new session.
                        conn.docs.remove(&id);
                        self.deliver(
                            id,
                            DbEvent::Notice(
                                "Connection lost. The next statement opens a new session; uncommitted changes were rolled back."
                                    .into(),
                            ),
                        );
                    }
                    SessionKey::Main => conn.reconnect_main(&self.notify),
                    // The next request opens a new metadata session.
                    SessionKey::Meta => {}
                }
                self.log(format!("{name}: connection lost, reconnecting"));
            }
            match key {
                // The metadata session's answers go where the main
                // session's would: to a document or the browser.
                SessionKey::Main | SessionKey::Meta => self.main_event(&name, event),
                SessionKey::Doc(id) => match event {
                    DbEvent::TaskDone { id: task, result }
                        if self.debug_session_task == Some(task) =>
                    {
                        self.debug_session_task = None;
                        match result
                            .map(|any| any.downcast::<pythia_debugger::sql::TargetSession>())
                        {
                            Ok(Ok(session)) => self.debug.set_target_session(*session),
                            Ok(Err(_)) => {}
                            Err(e) => self.log(format!(
                                "Debugger: the SQL panel can't tell which session is debugged: {}",
                                e.text()
                            )),
                        }
                    }
                    DbEvent::TaskDone { id: task, result }
                        if self.debug_run.is_some_and(|(_, t)| t == task) =>
                    {
                        self.debug_run = None;
                        let outcome = result.and_then(|any| {
                            any.downcast::<pythia_debugger::target::DebugOutcome>()
                                .map_err(|_| pythia_db::DbError::new("unexpected debug result"))
                        });
                        let (connected, error) = match outcome {
                            Ok(outcome) => {
                                if !outcome.output.is_empty() {
                                    self.deliver(id, DbEvent::Output(outcome.output));
                                }
                                (outcome.connected, outcome.error)
                            }
                            Err(e) => (true, Some(e)),
                        };
                        match error {
                            None => self.log("Debugged block finished"),
                            Some(e) => {
                                // The debugger waits for a session that
                                // won't come (#123).
                                if !connected {
                                    let actions = self.debug.target_failed(&e);
                                    self.debug_actions(actions);
                                }
                                self.log(format!("Debugged block failed: {e}"));
                                self.deliver(id, DbEvent::Error(e));
                            }
                        }
                    }
                    event => self.deliver(id, event),
                },
            }
        }
    }

    fn main_event(&mut self, name: &str, event: DbEvent) {
        match event {
            DbEvent::Connected(server) => {
                let schema = match (server.dialect.kind(), server.current_schema.as_str()) {
                    (pythia_db::DbKind::MySql, "") => "no database".to_owned(),
                    (pythia_db::DbKind::MySql, db) => format!("database {db}"),
                    (_, schema) => format!("schema {schema}"),
                };
                let line = format!(
                    "Connected to {name} ({} {}, {schema})",
                    server.dialect.name(),
                    server.version,
                );
                let schema = server.current_schema.clone();
                self.catalogs.insert(
                    name.to_owned(),
                    pythia_lsp_core::CatalogCache::new(schema.clone())
                        .with_names(name_style(server.dialect)),
                );
                self.catalog_requested.retain(|(c, _)| c != name);
                if let Some(conn) = self.conns.get_mut(name) {
                    conn.status = crate::connections::Status::Connected(server);
                }
                self.log(line);
                // Counts sent to an earlier session won't come back.
                self.invalid_inflight.retain(|_, c| c != name);
                self.refresh_invalid(name);
                if self.connecting.as_deref() == Some(name) {
                    self.connecting = None;
                    self.connect_dialog.connected();
                    if self.settings.switch_window_connection {
                        self.attach_browser(name.to_owned());
                        // Bind unbound windows to the first connection.
                        for doc in self.docs.values_mut().filter(|d| d.connection.is_none()) {
                            doc.connection = Some(name.to_owned());
                        }
                    } else {
                        // The new connection is the active one.
                        self.activate(name.to_owned());
                    }
                    let reload: Vec<DocId> = self
                        .docs
                        .iter()
                        .filter(|(_, d)| {
                            d.connection.as_deref() == Some(name)
                                && !matches!(d.kind, DocKind::Sql(_))
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    let dialect = self.dialect_of(Some(name));
                    for id in reload {
                        let effects = self.docs.get_mut(&id).expect("doc").on_attach(dialect);
                        self.apply(Some(id), effects);
                    }
                }
            }
            DbEvent::ConnectFailed(e) => {
                warn!("connect to {name} failed: {e}");
                self.conns.close(name);
                if self.connecting.as_deref() == Some(name) {
                    self.connecting = None;
                    if let Some(key) = &e.host_key {
                        self.connect_dialog
                            .failed_host_key(e.to_string(), (**key).clone());
                    } else if e.is_missing_client() {
                        self.connect_dialog.failed_missing_client(e.to_string());
                    } else {
                        self.connect_dialog.failed(e.to_string());
                    }
                } else if e.is_missing_client() {
                    self.log(format!(
                        "{name}: the Oracle Instant Client isn't installed (Connect… shows how): {e}"
                    ));
                } else {
                    self.log(format!("{name}: {e}"));
                }
            }
            DbEvent::TaskDone { id, result } if self.learn_inflight == Some(id) => {
                self.learn_inflight = None;
                type Learned = (String, Option<pythia_plsql::FormatOptions>);
                match result.map(|any| any.downcast::<Learned>()) {
                    Ok(Ok(learned)) => {
                        let (from, options) = *learned;
                        self.settings_dialog.learned(from, options);
                    }
                    Ok(Err(_)) => {}
                    Err(e) => {
                        self.log(format!("Reading the schema's source: {e}"));
                        self.settings_dialog.learned("the schema".into(), None);
                    }
                }
            }
            DbEvent::TaskDone { id, result } if self.debug_sql_inflight.contains_key(&id) => {
                let lookup = self.debug_sql_inflight.remove(&id).expect("checked");
                type Found = Result<Vec<pythia_debugger::sql::SessionSql>, String>;
                type Plan = Result<pythia_debugger::sql::CursorPlan, String>;
                match lookup {
                    DebugSqlLookup::Find(stop) => {
                        let found = match result.map(|any| any.downcast::<Found>()) {
                            Ok(Ok(found)) => *found,
                            Ok(Err(_)) => Err("unexpected result".into()),
                            Err(e) => Err(e.text()),
                        };
                        self.debug.sql_found(stop, found);
                    }
                    DebugSqlLookup::Plan(sql_id, child) => {
                        let plan = match result.map(|any| any.downcast::<Plan>()) {
                            Ok(Ok(plan)) => *plan,
                            Ok(Err(_)) => Err("unexpected result".into()),
                            Err(e) => Err(e.text()),
                        };
                        self.debug.plan_loaded(sql_id, child, plan);
                    }
                }
            }
            DbEvent::TaskDone { id, result } if self.opaque_inflight.contains_key(&id) => {
                let unit = self.opaque_inflight.remove(&id).expect("checked");
                match result.map(|any| any.downcast::<Vec<(String, String)>>()) {
                    Ok(Ok(reasons)) => self.debug.set_opaque_reasons(&unit, *reasons),
                    Ok(Err(_)) => {}
                    Err(e) => warn!("explaining opaque variables of {unit}: {e}"),
                }
            }
            DbEvent::TaskDone { id, result } if self.invalid_inflight.contains_key(&id) => {
                let connection = self.invalid_inflight.remove(&id).unwrap_or_default();
                match result.map(|any| any.downcast::<Vec<(String, u32)>>()) {
                    Ok(Ok(per_schema)) => {
                        self.invalid.insert(
                            connection.clone(),
                            InvalidCount {
                                per_schema: *per_schema,
                                at: std::time::Instant::now(),
                            },
                        );
                    }
                    Ok(Err(_)) => {}
                    Err(e) => warn!("counting invalid objects on {connection}: {e}"),
                }
                if self.invalid_again.remove(&connection) {
                    self.refresh_invalid(&connection);
                }
            }
            DbEvent::TaskDone { id, result } if self.catalog_inflight.contains_key(&id) => {
                let connection = self.catalog_inflight.remove(&id).unwrap_or_default();
                match result.map(|any| any.downcast::<crate::catalog::Answer>()) {
                    Ok(Ok(answer)) => {
                        if let Some(cache) = self.catalogs.get_mut(&connection) {
                            answer.apply(cache);
                            self.catalog_revision += 1;
                        }
                    }
                    Ok(Err(_)) => warn!("unexpected catalog task result"),
                    Err(e) => warn!("catalog lookup failed: {e}"),
                }
            }
            DbEvent::TaskDone { id, result } if self.assistant.is_lookup(id) => {
                let parts = result.map_err(|e| e.to_string()).and_then(|any| {
                    any.downcast::<Vec<pythia_ai::Part>>()
                        .map(|parts| *parts)
                        .map_err(|_| "unexpected lookup result".to_owned())
                });
                self.assistant
                    .on_lookup(parts, &self.settings.ai, self.notify.callback());
            }
            DbEvent::Meta { id, result } if self.finder.pending == Some(id) => match result {
                Ok(MetaResult::Objects(objects)) => self.finder.results(id, objects),
                Ok(_) => {}
                Err(e) => {
                    self.finder.results(id, Vec::new());
                    self.log(format!("Find object: {e}"));
                }
            },
            event => {
                let id = event_id(&event);
                if let Some(doc) = id.and_then(|id| self.routes.remove(&id)) {
                    self.deliver(doc, event);
                } else if id.is_some_and(|id| self.browser.owns(id)) {
                    let mut log = Vec::new();
                    let effects = self.browser.on_event(event, &mut log);
                    for line in log {
                        self.log(line);
                    }
                    self.apply_browser(effects);
                }
            }
        }
    }

    // ----- ui -------------------------------------------------------------

    fn menu_bar(
        &mut self,
        ui: &mut Ui,
        commands: &mut Vec<Command>,
        open_report: &mut Option<(String, String)>,
    ) {
        let variables = self.variables_state();
        let keymap = &self.keymap;
        let user_reports = &self.user_reports;
        let mut delete = None;
        let ctx = ui.ctx().clone();
        let autocommit = self.settings.autocommit;
        let dialect = self.dialect();
        let mut item = |ui: &mut Ui, command: Command| {
            // Variables only with some to show, ticked while they show (#168).
            let (enabled, selected) = match command {
                Command::ToggleAutocommit => (true, autocommit),
                Command::ShowVariables => match variables {
                    Some((n, shown)) => (n > 0, n > 0 && shown),
                    None => (false, false),
                },
                _ => (true, false),
            };
            let button = egui::Button::new(command.short_label())
                .selected(selected)
                .shortcut_text(keymap.shortcut_text(&ctx, command));
            let mut response = ui.add_enabled(enabled, button);
            if command == Command::ShowVariables {
                response = response.on_disabled_hover_text(match variables {
                    Some(_) => NO_VARIABLES,
                    None => NO_SQL_WINDOW,
                });
            }
            if response.clicked() {
                commands.push(command);
            }
        };
        // Commands in groups between separators; what the connection's
        // database can't do is left out.
        let mut groups = |ui: &mut Ui, groups: &[&[Command]]| {
            let mut first = true;
            for group in groups {
                let shown: Vec<Command> = group
                    .iter()
                    .copied()
                    .filter(|c| c.available(dialect))
                    .collect();
                if shown.is_empty() {
                    continue;
                }
                if !first {
                    ui.separator();
                }
                first = false;
                for command in shown {
                    item(ui, command);
                }
            }
        };
        egui::MenuBar::new().ui(ui, |ui| {
            bar_menu(ui, "File", |ui| {
                groups(ui, &[&[Command::NewSqlWindow]]);
                if Command::NEW_OBJECTS
                    .iter()
                    .any(|(c, _)| c.available(dialect))
                {
                    ui.menu_button("New Object", |ui| {
                        // Tables and views, code and jobs, then links and
                        // accounts.
                        let objects: Vec<Command> =
                            Command::NEW_OBJECTS.iter().map(|(c, _)| *c).collect();
                        let at = |t| {
                            Command::NEW_OBJECTS
                                .iter()
                                .position(|(_, o)| *o == t)
                                .unwrap_or(objects.len())
                        };
                        let code = at(pythia_db::ObjectType::Trigger);
                        let links = at(pythia_db::ObjectType::DatabaseLink);
                        groups(
                            ui,
                            &[&objects[..code], &objects[code..links], &objects[links..]],
                        );
                    });
                }
                groups(
                    ui,
                    &[
                        &[Command::OpenFile, Command::SaveFile, Command::SaveFileAs],
                        &[
                            Command::CloseTab,
                            Command::CloseOtherTabs,
                            Command::ReopenClosed,
                        ],
                        &[Command::Settings],
                        &[Command::Quit],
                    ],
                )
            });
            bar_menu(ui, "Edit", |ui| {
                groups(
                    ui,
                    &[
                        &[Command::Format, Command::RenameSymbol, Command::History],
                        &[Command::FoldAll, Command::UnfoldAll],
                    ],
                )
            });
            bar_menu(ui, "Session", |ui| {
                groups(
                    ui,
                    &[
                        &[Command::Connect, Command::Disconnect],
                        &[
                            Command::ExecuteAll,
                            Command::ExecuteCurrent,
                            Command::ExplainPlan,
                            Command::ExecuteWithStatistics,
                            Command::ExecuteToFile,
                            Command::Profile,
                            Command::Cancel,
                        ],
                        &[
                            Command::Commit,
                            Command::Rollback,
                            Command::ToggleAutocommit,
                        ],
                    ],
                )
            });
            bar_menu(ui, "Tools", |ui| {
                groups(
                    ui,
                    &[
                        &[
                            Command::FindObject,
                            Command::FindInSource,
                            Command::SearchData,
                        ],
                        &[
                            Command::SchedulerJobs,
                            Command::InvalidObjects,
                            Command::MaintenanceScan,
                        ],
                        &[Command::ImportData, Command::ExportTables],
                        &[Command::CompareSchemas, Command::CompareData],
                    ],
                )
            });
            bar_menu(ui, "AI", |ui| {
                groups(
                    ui,
                    &[
                        &[Command::AiAsk, Command::AiGenerateSql],
                        &[
                            Command::AiExplain,
                            Command::AiFix,
                            Command::AiRewrite,
                            Command::AiAnalyzePlan,
                            Command::AiFormat,
                        ],
                        &[Command::ShowAssistant],
                    ],
                )
            });
            if dialect.supports(Feature::Debugger) {
                bar_menu(ui, "Debug", |ui| {
                    groups(
                        ui,
                        &[
                            &[Command::DebugStart, Command::ToggleBreakpoint],
                            &[
                                Command::DebugContinue,
                                Command::DebugStepOver,
                                Command::DebugStepInto,
                                Command::DebugStepOut,
                                Command::DebugRunToCursor,
                                Command::DebugStop,
                            ],
                            &[Command::ShowDebugger, Command::DebugSetup],
                        ],
                    )
                });
            }
            let reports = dialect.supports(Feature::Reports);
            if reports {
                bar_menu(ui, "Reports", |ui| {
                    for category in crate::reports::categories() {
                        ui.menu_button(category, |ui| {
                            for report in crate::reports::REPORTS
                                .iter()
                                .filter(|r| r.category == category)
                            {
                                if ui.button(report.name).clicked() {
                                    *open_report = Some((report.name.into(), report.sql.into()));
                                    ui.close();
                                }
                            }
                        });
                    }
                    if !user_reports.reports.is_empty() {
                        ui.separator();
                        for category in user_reports.categories() {
                            let label = if category.is_empty() {
                                "My reports"
                            } else {
                                category
                            };
                            ui.menu_button(label, |ui| {
                                for (i, report) in user_reports.reports.iter().enumerate() {
                                    if report.category != category {
                                        continue;
                                    }
                                    let r = ui.button(&report.name);
                                    if r.clicked() {
                                        *open_report =
                                            Some((report.name.clone(), report.sql.clone()));
                                        ui.close();
                                    }
                                    r.context_menu(|ui| {
                                        if ui.button("Delete report").clicked() {
                                            delete = Some(i);
                                            ui.close();
                                        }
                                    });
                                }
                            });
                        }
                    }
                    ui.separator();
                    groups(ui, &[&[Command::SaveAsReport]]);
                });
            }
            if dialect.supports(Feature::Dashboard) || dialect.supports(Feature::Monitor) {
                bar_menu(ui, "Monitor", |ui| {
                    groups(
                        ui,
                        &[
                            &[
                                Command::MonitorDashboard,
                                Command::MonitorSessions,
                                Command::MonitorBlocking,
                            ],
                            &[Command::MonitorAwr, Command::MonitorTrace],
                        ],
                    )
                });
            }
            bar_menu(ui, "View", |ui| {
                groups(
                    ui,
                    &[
                        &[
                            Command::ShowPalette,
                            Command::ShowBrowser,
                            Command::ShowLog,
                            Command::ShowAssistant,
                            Command::ShowVariables,
                        ],
                        &[
                            Command::ThemeSystem,
                            Command::ThemeDark,
                            Command::ThemeLight,
                        ],
                        &[Command::ZoomIn, Command::ZoomOut, Command::ZoomReset],
                        &[Command::ResetLayout],
                    ],
                )
            });
            bar_menu(ui, "Help", |ui| groups(ui, &[&[Command::About]]));
        });
        if let Some(i) = delete {
            let report = self.user_reports.reports.remove(i);
            self.log(format!("Deleted report {}", report.name));
            self.save_user_reports();
        }
    }

    /// The toolbar. Buttons follow the focused window and its session, and
    /// say why they're disabled. Returns a connection picked for the
    /// focused window.
    fn toolbar(&self, ui: &mut Ui, commands: &mut Vec<Command>) -> Option<ConnectionChoice> {
        let focused_id = self.focused;
        let focused = focused_id.and_then(|id| self.docs.get(&id));
        let running = focused.is_some_and(Document::is_busy);
        // Execute runs a statement that waits for values.
        let waiting =
            focused.is_some_and(|d| matches!(&d.kind, DocKind::Sql(w) if w.waiting_for_values()));
        let is_sql = focused.is_some_and(|d| matches!(d.kind, DocKind::Sql(_)));
        let is_program = focused.is_some_and(|d| matches!(d.kind, DocKind::Program(_)));
        let connection = self.current_connection();
        let conn = connection.as_deref().and_then(|c| self.conns.get(c));
        let connected = conn.is_some_and(|c| c.is_connected());
        let session = self.focused_doc_session();
        let pending = session.is_some_and(|s| s.pending_tx);
        let ctx = ui.ctx().clone();
        let tip = |c: Command| match self.keymap.shortcut(c) {
            Some(s) => format!("{} ({})", c.short_label(), ctx.format_shortcut(s)),
            None => c.short_label().to_owned(),
        };
        let need_connection = || {
            if connected {
                Ok(())
            } else {
                Err("Not connected")
            }
        };
        let mut choice = None;
        let dialect = self.dialect();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            // Commands the connection's database can't do get no button.
            let button =
                |ui: &mut Ui, icon: Icon, text: &str, command: Command, state: Result<(), &str>| {
                    if !command.available(dialect) {
                        return None;
                    }
                    tool_button(ui, icon, text, &tip(command), state, Emphasis::Normal)
                        .then_some(command)
                };

            // Files.
            commands.extend(button(
                ui,
                Icon::NewSql,
                "New SQL",
                Command::NewSqlWindow,
                Ok(()),
            ));
            ui.menu_button(RichText::new("⏷").small(), |ui| {
                for command in [
                    Command::OpenFile,
                    Command::ReopenClosed,
                    Command::NewSqlWindow,
                ] {
                    let item = egui::Button::new(command.short_label())
                        .shortcut_text(self.keymap.shortcut_text(&ctx, command));
                    if ui.add(item).clicked() {
                        commands.push(command);
                    }
                }
            })
            .response
            .on_hover_text("Open a file, reopen a closed window");
            commands.extend(button(
                ui,
                Icon::Save,
                "",
                Command::SaveFile,
                if is_sql || is_program {
                    Ok(())
                } else {
                    Err("Only SQL and program windows are saved to files")
                },
            ));
            ui.separator();

            // The focused window's connection.
            match conn {
                None => {
                    if tool_button(
                        ui,
                        Icon::Connect,
                        "Connect…",
                        &tip(Command::Connect),
                        Ok(()),
                        Emphasis::Primary,
                    ) {
                        commands.push(Command::Connect);
                    }
                }
                Some(conn) => {
                    let name = conn.name();
                    let color =
                        connection_color(&conn.profile).unwrap_or(ui.visuals().text_color());
                    let (icon, label) = connection_label(conn);
                    let title = (
                        icon.image(ui),
                        RichText::new(format!("{label}  ⏷")).color(color),
                    );
                    let response = egui::containers::menu::MenuButton::new(title)
                        .ui(ui, |ui| {
                            if is_sql
                                && self.settings.switch_window_connection
                                && let Some(id) = focused_id
                            {
                                ui.weak("Connection of this window");
                                for other in self.conns.open.keys() {
                                    let item = ui.add_enabled(
                                        !pending || *other == name,
                                        egui::Button::selectable(*other == name, other),
                                    );
                                    let item = item.on_disabled_hover_text(
                                        "Commit or roll back this window's transaction first",
                                    );
                                    if item.clicked() && *other != name {
                                        choice = Some(ConnectionChoice::Window(id, other.clone()));
                                    }
                                }
                                ui.separator();
                            } else if !self.settings.switch_window_connection
                                && self.conns.open.len() > 1
                            {
                                // The app's connection: SQL windows and the
                                // object browser move with it.
                                ui.weak("Active connection");
                                let active = self.active.as_deref();
                                for other in self.conns.open.keys() {
                                    let on = active == Some(other.as_str());
                                    if ui.add(egui::Button::selectable(on, other)).clicked() && !on
                                    {
                                        choice = Some(ConnectionChoice::Active(other.clone()));
                                    }
                                }
                                ui.separator();
                            }
                            for (command, text) in [
                                (Command::Connect, "Connect…".to_owned()),
                                (Command::Disconnect, format!("Disconnect {name}")),
                            ] {
                                let item = egui::Button::new(text)
                                    .shortcut_text(self.keymap.shortcut_text(&ctx, command));
                                if ui.add(item).clicked() {
                                    commands.push(command);
                                }
                            }
                        })
                        .0;
                    let hover = match conn.server() {
                        Some(server) => connection_hover(conn.dialect(), server),
                        None => format!("Connecting to {name}…"),
                    };
                    response.on_hover_text(hover);
                }
            }
            ui.separator();

            // Running statements, or compiling in a program window.
            let not_running = if !is_sql && !is_program {
                Err("This window doesn't run statements")
            } else if running && !waiting {
                Err("Running")
            } else {
                need_connection()
            };
            if is_program {
                let read_only = conn.is_some_and(|c| c.profile.read_only);
                let state = if read_only {
                    Err("The connection is read-only")
                } else {
                    not_running
                };
                if tool_button(
                    ui,
                    Icon::Compile,
                    "Compile",
                    &tip(Command::ExecuteAll).replace("Execute (selection or all)", "Compile"),
                    state,
                    Emphasis::Normal,
                ) {
                    commands.push(Command::ExecuteAll);
                }
            } else {
                commands.extend(button(
                    ui,
                    Icon::Execute,
                    "Execute",
                    Command::ExecuteAll,
                    not_running,
                ));
            }
            let sql_only = |state: Result<(), &'static str>| {
                if is_sql {
                    state
                } else {
                    Err("Only in SQL windows")
                }
            };
            commands.extend(button(
                ui,
                Icon::ExecuteCurrent,
                "",
                Command::ExecuteCurrent,
                sql_only(not_running),
            ));
            commands.extend(button(
                ui,
                Icon::Explain,
                "",
                Command::ExplainPlan,
                sql_only(if running {
                    Err("Running")
                } else {
                    need_connection()
                }),
            ));
            commands.extend(button(
                ui,
                Icon::Bug,
                "",
                Command::DebugStart,
                self.debug_state(),
            ));
            commands.extend(button(
                ui,
                Icon::Stop,
                "",
                Command::Cancel,
                if running {
                    Ok(())
                } else {
                    Err("Nothing is running")
                },
            ));
            ui.separator();

            // The focused window's transaction.
            let transaction = self.transaction_state();
            for (icon, command) in [
                (Icon::Commit, Command::Commit),
                (Icon::Rollback, Command::Rollback),
            ] {
                let emphasis = if pending {
                    Emphasis::Warn
                } else {
                    Emphasis::Normal
                };
                if tool_button(ui, icon, "", &tip(command), transaction, emphasis) {
                    commands.push(command);
                }
            }
            ui.separator();

            // Tools.
            commands.extend(button(
                ui,
                Icon::Text,
                "",
                Command::Format,
                if is_sql || is_program {
                    Ok(())
                } else {
                    Err("Only in SQL and program windows")
                },
            ));
            commands.extend(button(
                ui,
                Icon::Search,
                "",
                Command::FindObject,
                need_connection(),
            ));
            commands.extend(button(
                ui,
                Icon::History,
                "",
                Command::History,
                if is_sql {
                    Ok(())
                } else {
                    Err("Recalls statements into a SQL window")
                },
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                commands.extend(button(ui, Icon::Palette, "", Command::ShowPalette, Ok(())));
                commands.extend(button(
                    ui,
                    Icon::Sparkle,
                    "",
                    Command::ShowAssistant,
                    Ok(()),
                ));
            });
        });
        choice
    }

    fn status_bar(&self, ui: &mut Ui, commands: &mut Vec<Command>) {
        ui.horizontal(|ui| {
            let focused = self.focused.and_then(|id| self.docs.get(&id).map(|d| (id, d)));
            let connection = self.current_connection();
            match connection.as_deref().and_then(|c| self.conns.get(c)) {
                None => {
                    icon_label(ui, Icon::Disconnect, RichText::new("Not connected").weak());
                }
                Some(conn) => {
                    let name = conn.name();
                    let color = connection_color(&conn.profile).unwrap_or(ui.visuals().text_color());
                    match conn.server() {
                        None => {
                            ui.spinner();
                            ui.label(format!("Connecting to {name}…"));
                        }
                        Some(server) => {
                            let (icon, text) = connection_label(conn);
                            icon_label(ui, icon, RichText::new(text).color(color))
                                .on_hover_text(connection_hover(conn.dialect(), server));
                        }
                    }
                    let session = focused.and_then(|(id, _)| conn.docs.get(&id));
                    ui.separator();
                    if session.is_some_and(|s| s.pending_tx) {
                        icon_label(ui, Icon::Transaction, RichText::new("Transaction pending").color(theme::WARN))
                            .on_hover_text("Uncommitted changes in this window's session (F10 commit, Shift+F10 rollback)");
                    } else {
                        ui.weak("No transaction");
                    }
                    if focused.is_some_and(|(id, _)| self.autocommit_for(id, conn)) {
                        ui.separator();
                        ui.label("Auto-commit");
                    }
                    if conn.profile.read_only {
                        ui.separator();
                        icon_label(ui, Icon::Lock, "Read-only");
                    }
                    if let Some(count) = connection.as_ref().and_then(|c| self.invalid.get(c))
                        && count.total() > 0
                    {
                        ui.separator();
                        let tip = count
                            .per_schema
                            .iter()
                            .map(|(schema, n)| format!("{schema}: {n}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let r = ui
                            .add(
                                egui::Button::image_and_text(
                                    Icon::Warning.image(ui),
                                    RichText::new(format!("{} invalid", count.total()))
                                        .color(theme::WARN),
                                )
                                .frame_when_inactive(false),
                            )
                            .on_hover_text(format!("{tip}\nClick to see and recompile them"));
                        if r.clicked() {
                            commands.push(Command::InvalidObjects);
                        }
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some((_, doc)) = focused
                    && let DocKind::Sql(w) = &doc.kind
                    && w.path.is_some()
                {
                    ui.weak(format!("{} · {}", w.encoding.label(), w.line_ending.label()))
                        .on_hover_text("Saved the way the file was read");
                }
                if let Some((_, doc)) = focused {
                    let (line_col, status) = doc.status();
                    if let Some((line, col)) = line_col {
                        ui.separator();
                        ui.label(format!("Ln {line}, Col {col}"));
                    }
                    if let Some(status) = status {
                        ui.separator();
                        ui.label(status);
                    }
                }
            });
        });
    }

    fn file_dialog(&mut self, action: FileAction, file_name: Option<String>) {
        if self.file_dialog.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let open = matches!(action, FileAction::Open);
        std::thread::spawn(move || {
            let dialog = rfd::FileDialog::new()
                .add_filter(
                    "SQL and PL/SQL",
                    &[
                        "sql", "pks", "pkb", "prc", "fnc", "tps", "tpb", "trg", "vw", "pls", "plb",
                    ],
                )
                .add_filter("All files", &["*"]);
            let path = if open {
                dialog.pick_file()
            } else {
                dialog
                    .set_file_name(file_name.unwrap_or_default())
                    .save_file()
            };
            let _ = tx.send(path);
        });
        self.file_dialog = Some((rx, action));
    }

    fn poll_file_dialog(&mut self) {
        let Some((rx, action)) = &self.file_dialog else {
            return;
        };
        let Ok(path) = rx.try_recv() else { return };
        let action = *action;
        self.file_dialog = None;
        let Some(path) = path else { return };
        match action {
            FileAction::Open => self.open_file(path, None),
            FileAction::SaveAs(id) => {
                let result = match self.docs.get_mut(&id).map(|d| &mut d.kind) {
                    Some(DocKind::Sql(w)) => {
                        if self.settings.format_auto.on_save {
                            w.editor.format_all(&self.settings.format);
                        }
                        w.save_to(path.clone())
                    }
                    Some(DocKind::Program(w)) => std::fs::write(&path, w.active_source()),
                    _ => Ok(()),
                };
                match result {
                    Ok(()) => self.log(format!("Saved {}", path.display())),
                    Err(e) => self.log(format!("Could not save {}: {e}", path.display())),
                }
            }
        }
    }

    /// Data files go to an import window, SQL and PL/SQL files open in a
    /// SQL window.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let paths: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        for path in paths {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            match ext.as_str() {
                "csv" | "tsv" | "txt" | "xlsx" | "xls" | "xlsb" | "xlsm" | "ods" => {
                    let focused_import = self.focused.filter(|id| {
                        matches!(self.docs.get(id).map(|d| &d.kind), Some(DocKind::Import(_)))
                    });
                    let id = match focused_import {
                        Some(id) => id,
                        None => {
                            let connection = self.current_connection();
                            self.add_doc(connection, DocKind::Import(Default::default()))
                        }
                    };
                    if let Some(DocKind::Import(w)) = self.docs.get_mut(&id).map(|d| &mut d.kind) {
                        w.load(path);
                    }
                }
                _ => self.open_file(path, None),
            }
        }
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested && !self.quit_confirmed && self.conns.any_pending_tx() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm {
                action: ConfirmAction::Quit,
                title: "Uncommitted transaction".into(),
                text: "There are sessions with uncommitted changes. Exiting rolls them back."
                    .into(),
                yes: "Exit and roll back",
                other: None,
            });
        }
    }

    fn about_ui(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let modal = egui::Modal::new(egui::Id::new("about")).show(ctx, |ui| {
            ui.heading("Pythia");
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.weak(pythia_db::oracle_driver());
            ui.label("A fast, native Oracle PL/SQL IDE.");
            let mut problem = None;
            if let Some(paths) = &self.paths {
                ui.add_space(6.0);
                ui.weak(format!("Config: {}", paths.config_dir.display()));
                ui.horizontal(|ui| {
                    ui.weak(format!("Logs: {}", paths.log_dir().display()));
                    if ui
                        .small_button("Open log folder")
                        .on_hover_text("The log and crash reports, e.g. for a bug report")
                        .clicked()
                        && let Err(e) = crate::desktop::open_path(&paths.log_dir())
                    {
                        problem = Some(format!("Could not open the log folder: {e}"));
                    }
                });
            }
            if let Some(problem) = problem {
                self.log(problem);
            }
            ui.add_space(6.0);
            if ui.button("Close").clicked() {
                self.about_open = false;
            }
        });
        if modal.should_close() {
            self.about_open = false;
        }
    }

    fn popups(&mut self, ctx: &egui::Context, commands: &mut Vec<Command>) {
        match self.connect_dialog.ui(ctx, &self.saved) {
            Some(crate::dialogs::ConnectAction::Connect(request)) => {
                self.connect(request.profile, request.passwords, request.save_profile);
            }
            Some(crate::dialogs::ConnectAction::Trust { key, then }) => match key.trust() {
                Ok(()) => {
                    self.log(format!(
                        "Trusted the SSH host key of {} ({})",
                        key.host, key.fingerprint
                    ));
                    self.connect(then.profile, then.passwords, then.save_profile);
                }
                Err(e) => self.connect_dialog.failed(e.to_string()),
            },
            Some(crate::dialogs::ConnectAction::Save {
                profile,
                previous,
                passwords,
            }) => {
                let name = profile.display_name();
                if let Some(previous) = previous.filter(|p| *p != name) {
                    self.saved.profiles.retain(|p| p.display_name() != previous);
                    secrets::forget(&previous);
                    secrets::forget_ssh(&previous);
                }
                match &passwords {
                    Some(passwords) => {
                        secrets::store(&name, &passwords.database);
                        secrets::store_ssh(&name, &passwords.ssh);
                    }
                    None => {
                        secrets::forget(&name);
                        secrets::forget_ssh(&name);
                    }
                }
                self.saved.upsert(profile);
                self.save_connections();
            }
            Some(crate::dialogs::ConnectAction::Delete(name)) => {
                self.saved.profiles.retain(|p| p.display_name() != name);
                secrets::forget(&name);
                secrets::forget_ssh(&name);
                self.save_connections();
            }
            None => {}
        }
        self.save_report_ui(ctx);
        match self.settings_dialog.ui(ctx, &mut self.settings) {
            crate::settings_dialog::SettingsResult::Changed => {
                self.keymap =
                    Keymap::with_overrides(self.settings.keymap, &self.settings.keybindings);
                self.apply_settings(ctx);
            }
            crate::settings_dialog::SettingsResult::Closed => {
                self.keymap =
                    Keymap::with_overrides(self.settings.keymap, &self.settings.keybindings);
                self.apply_settings(ctx);
                self.save_settings();
            }
            crate::settings_dialog::SettingsResult::Learn(from) => self.learn_format(from),
            crate::settings_dialog::SettingsResult::DebugSetup => self.open_debug_setup(None),
            crate::settings_dialog::SettingsResult::None => {}
        }
        for action in self
            .debug_setup
            .ui(ctx, &self.settings, &self.saved.profiles)
        {
            match action {
                crate::debug_setup::SetupAction::SetHost(host) => {
                    self.log(format!("Debugger host is now {host}"));
                    self.settings.debug_host = host;
                }
                crate::debug_setup::SetupAction::SetPorts(ports) => {
                    self.log(format!("Debugger ports are now {ports}"));
                    self.settings.debug_ports = ports;
                }
            }
            self.save_settings();
        }
        if let Some(command) = self.palette.ui(ctx, &self.keymap, self.dialect()) {
            commands.push(command);
        }
        if let Some((part, range)) = self.symbol_picker.ui(ctx) {
            self.record_jump();
            if let Some(doc) = self.focused.and_then(|id| self.docs.get_mut(&id)) {
                doc.goto_position(part, range);
            }
        }
        match self.reference_list.ui(ctx) {
            Some(crate::dialogs::ReferenceAction::Show { doc, part, range }) => {
                // The place the list was opened from, once.
                if !self.reference_list.jumped {
                    self.reference_list.jumped = true;
                    self.record_jump();
                }
                if let Some(d) = self.docs.get_mut(&doc) {
                    d.goto_position(part, range);
                }
            }
            Some(crate::dialogs::ReferenceAction::Cancel) if self.reference_list.jumped => {
                self.navigate_history(true);
            }
            Some(crate::dialogs::ReferenceAction::Cancel) | None => {}
        }
        match self.finder.ui(ctx) {
            Some(FinderAction::Search(pattern)) => {
                if let Some(name) = self.current_connection() {
                    let id = request_id();
                    self.finder.pending = Some(id);
                    self.conns.send_main(
                        &name,
                        DbRequest::Meta {
                            id,
                            query: MetaQuery::Find {
                                pattern,
                                limit: 200,
                            },
                        },
                    );
                }
            }
            Some(FinderAction::Open(object)) => {
                if let Some(connection) = self.current_connection() {
                    self.open(if opens_as_program(object.object_type) {
                        OpenRequest::Program { connection, object }
                    } else {
                        OpenRequest::Object { connection, object }
                    });
                }
            }
            None => {}
        }
        let connection = self.current_connection();
        if let Some(sql) = self
            .history_dialog
            .ui(ctx, &self.history, connection.as_deref())
        {
            match self
                .focused
                .and_then(|id| self.docs.get_mut(&id))
                .and_then(Document::sql_window)
            {
                Some(w) => w.editor.insert(&sql),
                None => {
                    if let Some(connection) = connection {
                        self.open(OpenRequest::Sql {
                            connection,
                            text: sql,
                            run: false,
                        });
                    }
                }
            }
        }
        if let Some(confirm) = &self.confirm
            && let Some(answer) = confirm_ui(ctx, confirm)
        {
            let action = confirm.action.clone();
            self.confirm = None;
            if let ConfirmAction::CloseWindows(ids) = &action
                && answer != Answer::Cancel
            {
                for &id in ids {
                    self.conns.end_doc(id, answer == Answer::Yes);
                    self.remove_window(id);
                }
            } else if let ConfirmAction::Activate(name) = &action {
                match answer {
                    Answer::Cancel => {
                        // The browser may have gone ahead (an object action).
                        if let Some(active) = self.active.clone() {
                            self.attach_browser(active);
                        }
                    }
                    _ => self.switch_to(name.clone(), Some(answer == Answer::Yes)),
                }
            } else if answer == Answer::Yes {
                match action {
                    ConfirmAction::Commit => self.end_transaction(true),
                    ConfirmAction::Execute { doc, current_only } => {
                        self.production_confirmed = true;
                        self.execute(doc, current_only);
                    }
                    ConfirmAction::Disconnect(name) => self.disconnect(&name),
                    ConfirmAction::Quit => {
                        self.quit_confirmed = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ConfirmAction::CloseWindows(_) | ConfirmAction::Activate(_) => {}
                }
            }
        }
        self.about_ui(ctx);
    }
}

impl eframe::App for PythiaApp {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        theme::base(visuals).to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // The theme's background between the panels and the dock's areas,
        // whatever the renderer clears to (#64).
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            ctx.content_rect(),
            0.0,
            theme::base(ui.visuals()),
        );
        self.pump_events();
        self.refresh_invalid_periodically(&ctx);
        self.handle_dropped_files(&ctx);
        self.poll_file_dialog();
        self.handle_close_request(&ctx);

        let modal_open = self.connect_dialog.open
            || self.confirm.is_some()
            || self.about_open
            || self.palette.open
            || self.symbol_picker.open
            || self.reference_list.open
            || self.finder.open
            || self.history_dialog.open
            || self.settings_dialog.is_recording()
            || self.save_report.is_some()
            || self.assistant.typing(&ctx);
        if self.debug.is_active() && !modal_open {
            self.debug.handle_keys(&ctx);
        }
        let actions = self.debug.poll();
        self.debug_actions(actions);
        let mut commands = if modal_open {
            Vec::new()
        } else {
            self.keymap.triggered(&ctx)
        };
        // Mouse back and forward buttons.
        if !modal_open {
            let (back, forward) = ctx.input(|i| {
                (
                    i.pointer.button_pressed(egui::PointerButton::Extra1),
                    i.pointer.button_pressed(egui::PointerButton::Extra2),
                )
            });
            if back {
                commands.push(Command::NavigateBack);
            }
            if forward {
                commands.push(Command::NavigateForward);
            }
        }

        let mut report = None;
        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui, &mut commands, &mut report));
        if let Some(report) = report {
            match self.current_connection() {
                Some(connection) => {
                    // Built-in reports are written for 12.1+ dictionaries.
                    let sql = match self.conns.get(&connection).and_then(|c| c.server()) {
                        Some(server) => pythia_db::compat::for_release(&report.1, server.release),
                        None => report.1,
                    };
                    let id = self.add_doc(
                        Some(connection),
                        DocKind::Sql(SqlWindow::new(report.0, sql)),
                    );
                    self.execute(id, false);
                }
                None => self.connect_dialog.show(self.last_profile.as_ref()),
            }
        }
        let toolbar_choice = egui::Panel::top("toolbar")
            .frame(theme::toolbar_frame(ui.style()))
            .show(ui, |ui| self.toolbar(ui, &mut commands))
            .inner;
        egui::Panel::bottom("status")
            .frame(theme::status_frame(ui.style()))
            .show(ui, |ui| self.status_bar(ui, &mut commands));

        let before = self.focused;
        if let Some((_, Tab::Doc(id))) = self.dock.find_active_focused() {
            self.focused = Some(*id);
        }
        let switched = self.settings.switch_window_connection;
        if std::mem::replace(&mut self.windows_switched, switched)
            && !switched
            && let Some(connection) = self.browser.connection.clone()
        {
            self.activate(connection);
        }
        // Windows on connections of their own: the object browser shows the
        // focused window's.
        if self.focused != before
            && self.settings.switch_window_connection
            && let Some(connection) = self
                .focused
                .and_then(|id| self.docs.get(&id))
                .and_then(|d| d.connection.clone())
                .filter(|c| self.conns.get(c).is_some())
        {
            self.attach_browser(connection);
        }
        let connections: Vec<String> = self.conns.open.keys().cloned().collect();
        let colors: HashMap<String, Color32> = self
            .conns
            .open
            .iter()
            .filter_map(|(n, c)| connection_color(&c.profile).map(|col| (n.clone(), col)))
            .collect();
        let connected: HashMap<String, bool> = self
            .conns
            .open
            .iter()
            .map(|(n, c)| (n.clone(), c.is_connected()))
            .collect();
        let pending_tx: HashSet<DocId> = self
            .conns
            .open
            .values()
            .flat_map(|c| {
                c.docs
                    .iter()
                    .filter(|(_, s)| s.pending_tx)
                    .map(|(id, _)| *id)
            })
            .collect();
        let privileges: HashMap<String, pythia_config::Privilege> = self
            .conns
            .open
            .iter()
            .map(|(n, c)| (n.clone(), c.profile.privilege))
            .collect();
        let dialects: HashMap<String, Dialect> = self
            .conns
            .open
            .iter()
            .map(|(n, c)| (n.clone(), c.dialect()))
            .collect();
        let read_only: HashSet<String> = self
            .conns
            .open
            .iter()
            .filter(|(_, c)| c.profile.read_only)
            .map(|(n, _)| n.clone())
            .collect();
        let production: HashSet<String> = self
            .conns
            .open
            .iter()
            .filter(|(_, c)| c.profile.production)
            .map(|(n, _)| n.clone())
            .collect();
        self.assistant.poll();
        self.poll_ai_format();
        let focused_title = self
            .focused
            .and_then(|id| self.docs.get(&id))
            .map(Document::title);
        let mut viewer = Viewer {
            docs: &mut self.docs,
            browser: &mut self.browser,
            project: &mut self.project,
            project_actions: Vec::new(),
            log: &self.log,
            settings: &self.settings,
            connections: &connections,
            colors: &colors,
            privileges: &privileges,
            dialects: &dialects,
            read_only: &read_only,
            production: &production,
            connected: &connected,
            catalogs: &self.catalogs,
            catalog_revision: self.catalog_revision,
            debug: &mut self.debug,
            debug_actions: Vec::new(),
            assistant: &mut self.assistant,
            assistant_actions: Vec::new(),
            focused_title,
            effects: Vec::new(),
            browser_effects: Effects::default(),
            closed: Vec::new(),
            close_asked: Vec::new(),
            rebind: Vec::new(),
            pending_tx: &pending_tx,
            active: self.active.as_deref(),
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                DockArea::new(&mut self.dock)
                    .style(theme::dock_style(ui.style()))
                    .show_leaf_collapse_buttons(false)
                    .show_inside(ui, &mut viewer);
            });
        let Viewer {
            effects,
            browser_effects,
            project_actions,
            closed,
            close_asked,
            rebind,
            debug_actions,
            assistant_actions,
            ..
        } = viewer;
        // From the debugger panel, e.g. a call stack frame's source.
        self.debug_actions(debug_actions);
        self.sync_debug_sql();
        for (id, connection) in rebind {
            self.rebind(id, connection);
        }
        match toolbar_choice {
            Some(ConnectionChoice::Window(id, connection)) => self.rebind(id, connection),
            Some(ConnectionChoice::Active(connection)) => self.activate(connection),
            None => {}
        }
        for id in closed {
            self.close_doc(id);
        }
        if !close_asked.is_empty() {
            self.close_windows(close_asked);
        }
        for (id, effects) in effects {
            self.apply(Some(id), effects);
        }
        self.apply_browser(browser_effects);
        self.project_actions(project_actions);
        self.assistant_actions(assistant_actions);

        self.popups(&ctx, &mut commands);
        commands.append(&mut self.queued_commands);
        for command in commands {
            self.run_command(&ctx, command);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let workspace = Workspace {
            dock: self.dock.clone(),
            docs: std::mem::take(&mut self.docs),
            next_doc: self.next_doc,
            last_profile: self.last_profile.clone(),
            project: std::mem::take(&mut self.project),
            marks: std::mem::take(&mut self.browser.marks),
            debug: self.debug.saved(),
            debug_panels: true,
        };
        eframe::set_value(storage, WORKSPACE_KEY, &workspace);
        self.docs = workspace.docs;
        self.project = workspace.project;
        self.browser.marks = workspace.marks;
        self.save_history();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.conns.open.clear();
        self.save_settings();
        self.save_history();
    }
}

#[cfg(test)]
impl PythiaApp {
    pub fn test_connect(&mut self, profile: ConnectionProfile, password: String) {
        let passwords = Passwords {
            database: password,
            ssh: String::new(),
        };
        self.connect(profile, passwords, false);
    }

    /// Connects as the connect dialog does, which can ask to trust an SSH
    /// host key first (kept in `known_hosts`).
    pub fn test_connect_dialog(
        &mut self,
        profile: ConnectionProfile,
        passwords: Passwords,
        known_hosts: PathBuf,
    ) {
        self.known_hosts = known_hosts;
        self.connect_dialog.show(None);
        if let Some(crate::dialogs::ConnectAction::Connect(request)) =
            self.connect_dialog.test_submit(profile, passwords)
        {
            self.connect(request.profile, request.passwords, false);
        }
    }

    pub fn test_connected(&self) -> bool {
        self.connecting.is_none() && self.conns.open.values().any(|c| c.is_connected())
    }

    pub fn test_connected_to(&self, name: &str) -> bool {
        self.connecting.is_none() && self.conns.get(name).is_some_and(|c| c.is_connected())
    }

    pub fn test_rebind_focused(&mut self, connection: &str) {
        if let Some(id) = self.focused {
            self.rebind(id, connection.to_owned());
        }
    }

    /// The active connection, and the one the object browser shows.
    pub fn test_active_connection(&self) -> (Option<String>, Option<String>) {
        (self.active.clone(), self.browser.connection.clone())
    }

    /// The tab title of the window whose title starts with `prefix`.
    pub fn test_tab_title(&self, prefix: &str) -> Option<String> {
        let doc = self.docs.values().find(|d| d.title().starts_with(prefix))?;
        let elsewhere = elsewhere(doc, self.active.as_deref(), &self.settings);
        Some(doc.tab_title(None, elsewhere).text().to_owned())
    }

    /// The connection of each window, by title.
    pub fn test_window_connections(&self) -> Vec<(String, Option<String>)> {
        self.docs
            .values()
            .map(|d| (d.title(), d.connection.clone()))
            .collect()
    }

    pub fn test_focused_connection(&self) -> Option<String> {
        self.focused
            .and_then(|id| self.docs.get(&id))
            .and_then(|d| d.connection.clone())
    }

    pub fn test_open(&mut self, request: OpenRequest) {
        self.open(request);
    }

    pub fn test_focused_busy(&self) -> bool {
        self.focused
            .and_then(|id| self.docs.get(&id))
            .is_some_and(Document::is_busy)
    }

    pub fn test_close_focused(&mut self) {
        self.run_command(&self.notify.0.clone(), Command::CloseTab);
    }

    pub fn test_execute_focused(&mut self) {
        if let Some(id) = self.focused {
            self.execute(id, false);
        }
    }

    pub fn test_expand_browser(&mut self, types: &[pythia_db::ObjectType]) {
        let effects = self.browser.expand(types);
        self.apply_browser(effects);
    }

    pub fn test_monitor_window(&mut self) -> &mut crate::monitor_window::MonitorWindow {
        let id = self.focused.expect("focused document");
        match self.docs.get_mut(&id).map(|d| &mut d.kind) {
            Some(DocKind::Monitor(m)) => m,
            _ => panic!("not a monitor"),
        }
    }

    pub fn test_monitor_window_ref(&self) -> &crate::monitor_window::MonitorWindow {
        let id = self.focused.expect("focused document");
        match self.docs.get(&id).map(|d| &d.kind) {
            Some(DocKind::Monitor(m)) => m,
            _ => panic!("not a monitor"),
        }
    }

    pub fn test_sql_window(&mut self) -> &mut SqlWindow {
        let id = self.focused.expect("focused document");
        self.docs
            .get_mut(&id)
            .and_then(Document::sql_window)
            .expect("SQL window")
    }

    pub fn test_export_tables(&mut self) -> &mut crate::export_tables::ExportTables {
        let id = self.focused.expect("focused document");
        match self.docs.get_mut(&id).map(|d| &mut d.kind) {
            Some(DocKind::ExportTables(e)) => e,
            _ => panic!("not an export window"),
        }
    }

    pub fn test_object_view(&self) -> &ObjectView {
        let id = self.focused.expect("focused document");
        match self.docs.get(&id).map(|d| &d.kind) {
            Some(DocKind::Object(v)) => v,
            _ => panic!("not an object view"),
        }
    }

    /// Whether `connection` has opened its metadata session.
    pub fn test_meta_session(&self, connection: &str) -> bool {
        self.conns
            .get(connection)
            .is_some_and(|c| c.has_meta_session())
    }

    pub fn test_object_editor(&mut self) -> &mut crate::object_editor::ObjectEditor {
        let id = self.focused.expect("focused document");
        match self.docs.get_mut(&id).map(|d| &mut d.kind) {
            Some(DocKind::ObjectEditor(e)) => e,
            _ => panic!("not an object editor"),
        }
    }

    pub fn test_table_editor(&mut self) -> &mut crate::table_editor::TableEditor {
        let id = self.focused.expect("focused document");
        match self.docs.get_mut(&id).map(|d| &mut d.kind) {
            Some(DocKind::TableEditor(e)) => e,
            _ => panic!("not a table editor"),
        }
    }

    pub fn test_jobs_window(&mut self) -> &mut crate::jobs_window::JobsWindow {
        let id = self.focused.expect("focused document");
        match self.docs.get_mut(&id).map(|d| &mut d.kind) {
            Some(DocKind::Jobs(j)) => j,
            _ => panic!("not a jobs window"),
        }
    }

    pub fn test_sql_window_ref(&self) -> Option<&SqlWindow> {
        match &self.docs.get(&self.focused?)?.kind {
            DocKind::Sql(w) => Some(w),
            _ => None,
        }
    }

    /// Runs `f` on the focused SQL window with a context as a frame gives
    /// it (its connection's dialect), and applies what it asks for.
    pub fn test_on_sql_window(&mut self, f: impl FnOnce(&mut SqlWindow, &DocContext) -> Effects) {
        let id = self.focused.expect("focused document");
        let connection = self.docs[&id].connection.clone();
        let dialect = self.dialect_of(connection.as_deref());
        let read_only = self.read_only_of(connection.as_deref());
        let production = self.production_of(connection.as_deref());
        let settings = self.settings.clone();
        let cx = DocContext {
            id,
            settings: &settings,
            connected: true,
            fetch_size: 100,
            connection,
            catalog: None,
            catalog_revision: 0,
            connections: &[],
            debug: None,
            marks: None,
            privilege: None,
            dialect,
            read_only,
            production,
        };
        let effects = f(self.test_sql_window(), &cx);
        self.apply(Some(id), effects);
    }

    pub fn test_cx(&self) -> (DocId, Settings, Option<String>) {
        let id = self.focused.expect("focused document");
        (id, self.settings.clone(), self.docs[&id].connection.clone())
    }

    pub fn test_apply(&mut self, effects: Effects) {
        self.apply(self.focused, effects);
    }

    pub fn test_show_connections(&mut self, profiles: Vec<ConnectionProfile>) {
        self.saved.profiles = profiles;
        let first = self.saved.profiles.first().cloned();
        self.connect_dialog.show(first.as_ref());
    }

    pub fn test_close_connect(&mut self) {
        self.connect_dialog.open = false;
        self.saved.profiles.clear();
    }

    pub fn test_search_data(&mut self, value: &str, column_names: bool) {
        let id = self.focused.expect("focused");
        let cx = self.doc_context(id);
        let (settings, connection) = (cx.settings.clone(), cx.connection.clone());
        let catalog = connection.as_ref().and_then(|c| self.catalogs.get(c));
        let cx = DocContext {
            id,
            settings: &settings,
            connected: true,
            fetch_size: 100,
            connection,
            catalog,
            catalog_revision: 0,
            connections: &[],
            debug: None,
            marks: None,
            privilege: None,
            dialect: Dialect::ORACLE,
            read_only: false,
            production: false,
        };
        let Some(DocKind::DataSearch(s)) = self.docs.get_mut(&id).map(|d| &mut d.kind) else {
            panic!("not a data search");
        };
        s.value = value.to_owned();
        s.column_names = column_names;
        let effects = s.start(&cx);
        self.apply(Some(id), effects);
    }

    pub fn test_data_hits(&self) -> Vec<String> {
        match self
            .focused
            .and_then(|id| self.docs.get(&id))
            .map(|d| &d.kind)
        {
            Some(DocKind::DataSearch(s)) => s
                .hits()
                .iter()
                .map(|h| format!("{}.{}={}", h.table, h.column, h.count))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn test_search_source(&mut self, query: &str) {
        let id = self.focused.expect("focused");
        let Some(DocKind::Search(s)) = self.docs.get_mut(&id).map(|d| &mut d.kind) else {
            panic!("not a search document");
        };
        s.query = query.to_owned();
        let effects = s.search();
        self.apply(Some(id), effects);
    }

    pub fn test_import(&mut self, path: std::path::PathBuf, table: &str) {
        let connection = self.current_connection();
        let id = self.add_doc(connection, DocKind::Import(Default::default()));
        let Some(DocKind::Import(w)) = self.docs.get_mut(&id).map(|d| &mut d.kind) else {
            unreachable!()
        };
        w.load(path);
        let effects = w.test_new_table(table, true);
        self.apply(Some(id), effects);
    }

    pub fn test_import_message(&self) -> Option<(bool, String)> {
        match &self.docs.get(&self.focused?)?.kind {
            DocKind::Import(w) => w.test_message(),
            _ => None,
        }
    }

    pub fn test_debug_setup(&mut self, host: &str, unit: pythia_debugger::Unit, line: u32) {
        self.settings.debug_host = host.to_owned();
        self.debug.toggle_breakpoint(unit, line);
    }

    pub fn test_setup_window(&mut self) -> &mut crate::debug_setup::DebugSetup {
        &mut self.debug_setup
    }

    pub fn test_setup_window_ref(&self) -> &crate::debug_setup::DebugSetup {
        &self.debug_setup
    }

    pub fn test_debug(&self) -> &pythia_debugger::ui::DebugSession {
        &self.debug
    }

    pub fn test_debug_mut(&mut self) -> &mut pythia_debugger::ui::DebugSession {
        &mut self.debug
    }

    /// Opens a program window with this source, without a connection.
    pub fn test_program_with(&mut self, object: ObjectRef, spec: &str, body: &str) -> DocId {
        let mut w = ProgramWindow::new(object);
        for (part, text) in w.parts.iter_mut().zip([spec, body]) {
            part.editor.set_text(text);
            part.saved = text.to_owned();
        }
        self.add_doc(None, DocKind::Program(w))
    }

    pub fn test_reference_rows(&self) -> bool {
        self.reference_list.open
    }

    /// The focused program window's active editor, if one is focused.
    pub fn test_program_editor(&mut self) -> Option<&mut crate::editor::Editor> {
        match self.docs.get_mut(&self.focused?).map(|d| &mut d.kind) {
            Some(DocKind::Program(w)) => w.active_editor(),
            _ => None,
        }
    }

    pub fn test_scan_window(&mut self) -> &mut crate::maintenance::ScanWindow {
        let id = self.focused.expect("focused document");
        match self.docs.get_mut(&id).map(|d| &mut d.kind) {
            Some(DocKind::MaintenanceScan(m)) => m,
            _ => panic!("not a maintenance scan"),
        }
    }

    pub fn test_maintenance_window(&mut self) -> &mut crate::maintenance::MaintenanceWindow {
        let id = self.focused.expect("focused document");
        match self.docs.get_mut(&id).map(|d| &mut d.kind) {
            Some(DocKind::Maintenance(m)) => m,
            _ => panic!("not a maintenance window"),
        }
    }

    pub fn test_invalid_window(&self) -> Option<&crate::invalid_window::InvalidObjectsWindow> {
        match &self.docs.get(&self.focused?)?.kind {
            DocKind::Invalid(w) => Some(w),
            _ => None,
        }
    }

    /// Invalid objects counted for the status bar, if counted.
    pub fn test_invalid_total(&self, connection: &str) -> Option<u32> {
        self.invalid.get(connection).map(InvalidCount::total)
    }

    /// Counts the connection's invalid objects again, as after a change.
    pub fn test_refresh_invalid(&mut self, connection: &str) {
        self.refresh_invalid(connection);
    }

    pub fn test_settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    pub fn test_browser_selected(&self) -> Option<ObjectRef> {
        self.browser.selected().cloned()
    }

    pub fn test_transaction_state(&self) -> Result<(), &'static str> {
        self.transaction_state()
    }

    pub fn test_debug_state(&self) -> Result<(), &'static str> {
        self.debug_state()
    }

    pub fn test_has_doc_session(&self) -> bool {
        self.focused_doc_session().is_some()
    }

    pub fn test_debug_running(&self) -> bool {
        self.debug_run.is_some()
    }

    /// The focused editor's execution marker: the document's title and the
    /// marked line (1-based, with its text), and the cursor line.
    pub fn test_execution_marker(&self) -> (String, Option<(usize, String)>, usize) {
        let Some(doc) = self.focused.and_then(|id| self.docs.get(&id)) else {
            return (String::new(), None, 0);
        };
        let editor = match &doc.kind {
            DocKind::Program(w) => w.parts.get(w.active).map(|p| &p.editor),
            DocKind::Sql(w) => Some(&w.editor),
            _ => None,
        };
        let Some(editor) = editor else {
            return (doc.title(), None, 0);
        };
        let marker = editor.execution_line().map(|line| {
            let text = editor.text().lines().nth(line - 1).unwrap_or_default();
            (line, text.trim().to_owned())
        });
        (doc.title(), marker, editor.line_col().0)
    }

    pub fn test_settings(&mut self, section_formatter: bool) {
        self.settings_dialog.open = true;
        if section_formatter {
            self.settings_dialog.show_formatter();
        }
    }

    pub fn test_code_font(&mut self, ctx: &egui::Context, file: &str) {
        self.settings.editor.font_file = file.to_owned();
        self.apply_settings(ctx);
    }

    pub fn test_apply_settings(&mut self, ctx: &egui::Context) {
        self.apply_settings(ctx);
    }

    pub fn test_log(&self) -> &[String] {
        &self.log
    }

    pub fn test_settings_keyboard(&mut self) {
        self.settings_dialog.open = true;
        self.settings_dialog.show_keyboard();
        self.settings
            .keybindings
            .insert("Format".into(), "Alt+Shift+F".into());
    }

    pub fn test_close_settings(&mut self) {
        self.settings_dialog.open = false;
    }

    /// Opens Settings on its `i`th page; false past the last.
    pub fn test_settings_page(&mut self, i: usize) -> bool {
        self.settings_dialog.open = true;
        self.settings_dialog.show_page(i)
    }

    pub fn test_settings_open(&self) -> bool {
        self.settings_dialog.open
    }

    pub fn test_learning(&self) -> bool {
        self.learn_inflight.is_some()
    }

    pub fn test_open_file(&mut self, path: std::path::PathBuf) {
        self.open_file(path, None);
    }

    pub fn test_program(&mut self) -> &mut ProgramWindow {
        let id = self.focused.expect("focused");
        match self.docs.get_mut(&id).map(|d| &mut d.kind) {
            Some(DocKind::Program(w)) => w,
            _ => panic!("not a program window"),
        }
    }

    pub fn test_apply_focused(&mut self, effects: Effects) {
        self.apply(self.focused, effects);
    }

    /// Where `tab` is docked: its node, and whether it's in front there.
    pub fn test_tab(&self, tab: Tab) -> Option<(egui_dock::NodePath, bool)> {
        let path = self.dock.find_tab(&tab)?;
        let front = match &self.dock[path.node_path()] {
            egui_dock::Node::Leaf(leaf) => leaf.active == path.tab,
            _ => false,
        };
        Some((path.node_path(), front))
    }

    /// Closes `tab` as its close button does, for tabs that need no more.
    pub fn test_close_tab(&mut self, tab: Tab) {
        if let Some(path) = self.dock.find_tab(&tab) {
            self.dock.remove_tab(path);
        }
    }

    pub fn test_command(&mut self, command: Command) {
        let ctx = self.notify.0.clone();
        self.run_command(&ctx, command);
    }

    pub fn test_focused(&self) -> DocId {
        self.focused.expect("focused document")
    }

    pub fn test_focus(&mut self, id: DocId) {
        self.open_tab(Tab::Doc(id));
    }

    pub fn test_commit(&mut self) {
        self.end_transaction(true);
    }

    pub fn test_rollback(&mut self) {
        self.end_transaction(false);
    }

    pub fn test_pending_tx(&self) -> bool {
        self.conns.any_pending_tx()
    }

    pub fn test_expand_table(&mut self, table: ObjectRef) {
        let effects = self.browser.expand_table(table);
        self.apply_browser(effects);
    }

    pub fn test_project(&mut self) -> &mut crate::project::Project {
        &mut self.project
    }

    pub fn test_project_actions(&mut self, actions: Vec<crate::project::ProjectAction>) {
        self.project_actions(actions);
    }

    pub fn test_focused_title(&self) -> Option<String> {
        self.focused
            .and_then(|id| self.docs.get(&id))
            .map(|d| d.title())
    }

    pub fn test_browser(&mut self) -> &mut Browser {
        &mut self.browser
    }

    pub fn test_browser_loading(&self) -> bool {
        self.browser.is_loading()
    }

    /// Sets up one AI backend whose answers come from `responder`.
    pub fn test_ai(&mut self, responder: crate::ai::Responder) {
        self.settings.ai.backends = vec![pythia_config::AiBackend {
            name: "Fake model".into(),
            command: "fake".into(),
            ..pythia_config::AiBackend::new(pythia_config::AiKind::Cli, "fake".into())
        }];
        self.assistant.responder = Some(responder);
    }

    pub fn test_assistant(&mut self) -> &mut crate::ai::Assistant {
        &mut self.assistant
    }

    pub fn test_ai_answers(&self) -> usize {
        self.assistant.answers()
    }

    pub fn test_connect_dialog_open(&self) -> bool {
        self.connect_dialog.open
    }

    pub fn test_connect_dialog_error(&self) -> Option<&str> {
        self.connect_dialog.test_error()
    }

    pub fn test_settings_ai(&mut self) {
        self.settings_dialog.open = true;
        self.settings_dialog.show_ai();
    }
}

/// A connection as the toolbar and status bar show it: with its database on
/// MySQL ("No database" without one), and marked when it's production.
fn connection_label(conn: &Connection) -> (Icon, String) {
    let name = match conn.database_label() {
        Some(database) => format!("{} · {database}", conn.name()),
        None => conn.name(),
    };
    if conn.profile.production {
        (Icon::Production, format!("{name}  PRODUCTION"))
    } else {
        (Icon::Database, name)
    }
}

/// The server and the current schema (database on MySQL).
fn connection_hover(dialect: Dialect, server: &pythia_db::ServerInfo) -> String {
    let current = match (dialect == Dialect::MYSQL, server.current_schema.as_str()) {
        (true, "") => "No database".to_owned(),
        (true, db) => format!("Database {db}"),
        (false, schema) => format!("Schema {schema}"),
    };
    format!("{}\n{current}", server.banner)
}

/// The connection a window's tab names: one other than the active
/// connection, where a program window or object view was kept, unless
/// windows switch connections on their own.
fn elsewhere<'a>(doc: &'a Document, active: Option<&str>, settings: &Settings) -> Option<&'a str> {
    doc.connection
        .as_deref()
        .filter(|c| !settings.switch_window_connection && active != Some(*c))
}

/// A connection picked in the toolbar's connection menu.
enum ConnectionChoice {
    /// For the focused SQL window (windows switch connections on their own).
    Window(DocId, String),
    /// The active connection.
    Active(String),
}

struct Viewer<'a> {
    docs: &'a mut BTreeMap<DocId, Document>,
    browser: &'a mut Browser,
    project: &'a mut crate::project::Project,
    project_actions: Vec<crate::project::ProjectAction>,
    log: &'a [String],
    settings: &'a Settings,
    connections: &'a [String],
    colors: &'a HashMap<String, Color32>,
    privileges: &'a HashMap<String, pythia_config::Privilege>,
    dialects: &'a HashMap<String, Dialect>,
    /// Connections whose profile is read-only.
    read_only: &'a HashSet<String>,
    /// Connections whose profile is marked as production.
    production: &'a HashSet<String>,
    connected: &'a HashMap<String, bool>,
    catalogs: &'a HashMap<String, pythia_lsp_core::CatalogCache>,
    catalog_revision: u64,
    debug: &'a mut pythia_debugger::ui::DebugSession,
    debug_actions: Vec<pythia_debugger::ui::DebugAction>,
    assistant: &'a mut crate::ai::Assistant,
    assistant_actions: Vec<crate::ai::PanelAction>,
    /// Title of the focused document, which assistant questions are about.
    focused_title: Option<String>,
    effects: Vec<(DocId, Effects)>,
    browser_effects: Effects,
    closed: Vec<DocId>,
    /// Closed tabs with an open transaction: they stay until the user
    /// says whether to commit or roll back.
    close_asked: Vec<DocId>,
    /// Documents moved to another connection.
    rebind: Vec<(DocId, String)>,
    /// Documents whose session has a pending transaction.
    pending_tx: &'a HashSet<DocId>,
    /// The active connection.
    active: Option<&'a str>,
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(*tab)
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match tab {
            Tab::Browser => "Objects".into(),
            Tab::Project => "Project".into(),
            Tab::Log => "Log".into(),
            Tab::Debugger => "Debugger".into(),
            Tab::DebugVariables => "Variables".into(),
            Tab::DebugWatches => "Watches".into(),
            Tab::DebugBreakpoints => "Breakpoints".into(),
            Tab::DebugSql => sql_plan_title(self.debug.sql_flag_count()).into(),
            Tab::Assistant => "Assistant".into(),
            Tab::Doc(id) => match self.docs.get(id) {
                Some(doc) => {
                    let color = doc
                        .connection
                        .as_ref()
                        .and_then(|c| self.colors.get(c))
                        .copied();
                    let elsewhere = elsewhere(doc, self.active, self.settings);
                    doc.tab_title(color, elsewhere).into()
                }
                None => "?".into(),
            },
        }
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        match tab {
            Tab::Browser => {
                let effects = self.browser.ui(ui, self.connections);
                self.browser_effects.extend(effects);
            }
            Tab::Project => {
                let connection = self
                    .browser
                    .connection
                    .clone()
                    .filter(|c| self.connected.get(c).copied().unwrap_or(false));
                let dialect = connection
                    .as_ref()
                    .and_then(|c| self.dialects.get(c))
                    .copied()
                    .unwrap_or_default();
                let actions = self.project.ui(ui, connection.as_deref(), dialect);
                self.project_actions.extend(actions);
            }
            Tab::Debugger => {
                let actions = self.debug.stack_ui(ui);
                self.debug_actions.extend(actions);
            }
            Tab::DebugVariables => self.debug.variables_ui(ui),
            Tab::DebugWatches => self.debug.watches_ui(ui),
            Tab::DebugBreakpoints => self.debug.breakpoints_ui(ui),
            Tab::DebugSql => {
                let actions = self.debug.sql_ui(ui);
                self.debug_actions.extend(actions);
            }
            Tab::Assistant => {
                let cx = crate::ai::PanelContext {
                    ai: &self.settings.ai,
                    focused: self.focused_title.clone(),
                };
                let actions = self.assistant.ui(ui, &cx);
                self.assistant_actions.extend(actions);
            }
            Tab::Log => {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in self.log {
                            ui.label(RichText::new(line).monospace());
                        }
                    });
            }
            Tab::Doc(id) => {
                let Some(doc) = self.docs.get_mut(id) else {
                    return;
                };
                // Connection picker for SQL windows: to switch connections
                // if the settings allow it, else only for a window without
                // an open connection.
                let on_open = doc
                    .connection
                    .as_ref()
                    .is_some_and(|c| self.connections.contains(c));
                if matches!(doc.kind, DocKind::Sql(_))
                    && !self.connections.is_empty()
                    && (self.settings.switch_window_connection || !on_open)
                {
                    let current = doc.connection.clone().unwrap_or_else(|| "(none)".into());
                    let pending = self.pending_tx.contains(id);
                    // A panel: the tab has no margin (see `tab_style_override`).
                    egui::Panel::top(egui::Id::new(("doc-conn-row", *id))).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.weak("Connection");
                            egui::ComboBox::from_id_salt(("doc-conn", *id))
                                .selected_text(&current)
                                .show_ui(ui, |ui| {
                                    for c in self.connections {
                                        let item = ui
                                            .add_enabled(
                                                !pending || *c == current,
                                                egui::Button::selectable(*c == current, c),
                                            )
                                            .on_disabled_hover_text(
                                                "Commit or roll back this window's transaction first",
                                            );
                                        if item.clicked() && *c != current {
                                            self.rebind.push((*id, c.clone()));
                                        }
                                    }
                                });
                        });
                    });
                }
                let connected = doc
                    .connection
                    .as_ref()
                    .and_then(|c| self.connected.get(c))
                    .copied()
                    .unwrap_or(false);
                let cx = DocContext {
                    id: *id,
                    settings: self.settings,
                    connected,
                    fetch_size: self.settings.fetch_size,
                    connection: doc.connection.clone(),
                    catalog: doc.connection.as_ref().and_then(|c| self.catalogs.get(c)),
                    catalog_revision: self.catalog_revision,
                    connections: self.connections,
                    debug: Some(&*self.debug),
                    marks: Some(&self.browser.marks),
                    privilege: doc
                        .connection
                        .as_ref()
                        .and_then(|c| self.privileges.get(c))
                        .copied(),
                    dialect: doc
                        .connection
                        .as_ref()
                        .and_then(|c| self.dialects.get(c))
                        .copied()
                        .unwrap_or_default(),
                    read_only: doc
                        .connection
                        .as_ref()
                        .is_some_and(|c| self.read_only.contains(c)),
                    production: doc
                        .connection
                        .as_ref()
                        .is_some_and(|c| self.production.contains(c)),
                };
                let effects = doc.ui(ui, &cx);
                self.effects.push((*id, effects));
            }
        }
    }

    fn on_close(&mut self, tab: &mut Tab) -> egui_dock::widgets::tab_viewer::OnCloseResponse {
        if let Tab::Doc(id) = tab {
            if self.pending_tx.contains(id) {
                // Asked first: commit, roll back or keep it.
                self.close_asked.push(*id);
                return egui_dock::widgets::tab_viewer::OnCloseResponse::Ignore;
            }
            self.closed.push(*id);
        }
        // Closing the plan is hiding it, as with the SQL toggle.
        if *tab == Tab::DebugSql {
            self.debug.show_sql(false);
        }
        egui_dock::widgets::tab_viewer::OnCloseResponse::Close
    }

    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }

    /// SQL and program windows fill their tab to the edges: the editor
    /// needs no frame, and the panels around it bring their own margin.
    fn tab_style_override(&self, tab: &Tab, global: &TabStyle) -> Option<TabStyle> {
        let Tab::Doc(id) = tab else {
            return None;
        };
        let doc = self.docs.get(id)?;
        matches!(doc.kind, DocKind::Sql(_) | DocKind::Program(_)).then(|| {
            let mut style = global.clone();
            style.tab_body.inner_margin = egui::Margin::ZERO;
            style
        })
    }
}

/// The SQL plan tab's title, with the statements whose estimates are off.
fn sql_plan_title(flagged: usize) -> String {
    if flagged > 0 {
        format!("SQL plan ⚠ {flagged}")
    } else {
        "SQL plan".into()
    }
}

/// TrueType, OpenType or font collection signature; anything else would
/// make egui panic when laying out text.
fn is_font(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(..4),
        Some([0, 1, 0, 0] | b"OTTO" | b"true" | b"ttcf")
    )
}

/// Selects every occurrence of the local symbol at the caret, so typing
/// renames them all at once. Returns a message for the log.
fn rename_symbol(editor: &mut crate::editor::Editor) -> Option<String> {
    let cursor = editor.cursor();
    let refs = pythia_lsp_core::references(editor.text(), cursor);
    if refs.is_empty() {
        return Some(
            "Rename works for local variables, parameters, cursors, aliases and CTEs".into(),
        );
    }
    let name = editor.text()[refs[0].clone()].to_owned();
    editor.inner().select_ranges(&refs, cursor);
    Some(format!(
        "Renaming {} occurrences of {name}: type the new name, Esc when done",
        refs.len()
    ))
}

/// Formats the selection (or the whole text) as one undo step.
/// Statements of an editor the AI is laying out (AI: Format Unsupported
/// Statements).
struct AiFormatRun {
    doc: DocId,
    part: usize,
    jobs: Vec<AiFormatJob>,
}

struct AiFormatJob {
    range: std::ops::Range<usize>,
    /// The statement's text when asked; the answer replaces only this.
    original: String,
    job: crate::ai::Job,
    result: Option<Result<String, String>>,
}

fn format_editor(
    editor: &mut crate::editor::Editor,
    options: &pythia_plsql::FormatOptions,
) -> String {
    let text = editor.text().to_owned();
    let line = editor.line_col().0;
    let formatted = match editor.selection() {
        Some(sel) => pythia_plsql::format_range(&text, sel, options),
        None => pythia_plsql::format(&text, options),
    };
    if !formatted.changed {
        return "Already formatted".into();
    }
    // Replace only the changed middle, so undo and markers stay precise.
    let prefix = text
        .bytes()
        .zip(formatted.text.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let prefix = (0..=prefix)
        .rev()
        .find(|i| text.is_char_boundary(*i) && formatted.text.is_char_boundary(*i))
        .unwrap_or(0);
    let suffix = text[prefix..]
        .bytes()
        .rev()
        .zip(formatted.text[prefix..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = (0..=suffix)
        .rev()
        .find(|s| {
            text.is_char_boundary(text.len() - s)
                && formatted.text.is_char_boundary(formatted.text.len() - s)
        })
        .unwrap_or(0);
    let new_line_count = formatted.text.lines().count().max(1);
    editor.replace(
        prefix..text.len() - suffix,
        &formatted.text[prefix..formatted.text.len() - suffix],
        line.min(new_line_count),
    );
    "Formatted".into()
}

fn reports_file(paths: &Paths) -> std::path::PathBuf {
    paths.config_dir.join("reports.toml")
}

fn history_file(paths: &Paths) -> std::path::PathBuf {
    paths.data_dir.join("history.toml")
}

fn load_or_default<T: serde::de::DeserializeOwned + Default>(path: &std::path::Path) -> T {
    load_toml(path).unwrap_or_else(|e| {
        error!("{e:#}");
        T::default()
    })
}

/// Invalid objects of a connection per schema, and when they were counted.
struct InvalidCount {
    per_schema: Vec<(String, u32)>,
    at: std::time::Instant,
}

impl InvalidCount {
    fn total(&self) -> u32 {
        self.per_schema.iter().map(|(_, n)| n).sum()
    }
}

/// A place in an editor: the document, its part and a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    doc: DocId,
    part: usize,
    offset: usize,
}

/// Places jumps started from (go to definition, symbols, references, …):
/// Back returns there, Forward goes again.
#[derive(Default)]
struct NavHistory {
    back: Vec<Position>,
    forward: Vec<Position>,
}

impl NavHistory {
    const LIMIT: usize = 100;

    fn record(&mut self, position: Position) {
        if self.back.last() != Some(&position) {
            self.back.push(position);
            if self.back.len() > Self::LIMIT {
                self.back.remove(0);
            }
        }
        self.forward.clear();
    }
}

/// How much a toolbar button stands out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Emphasis {
    Normal,
    /// The main action when nothing else applies (e.g. Connect…).
    Primary,
    /// Needs attention (e.g. Commit with a pending transaction).
    Warn,
}

/// A toolbar button with an icon and an optional label, frameless unless
/// emphasized. `state` says why it's disabled; disabled buttons are dimmed,
/// colored icons included. Returns whether it was clicked.
fn tool_button(
    ui: &mut Ui,
    icon: Icon,
    text: &str,
    tip: &str,
    state: Result<(), &str>,
    emphasis: Emphasis,
) -> bool {
    let enabled = state.is_ok();
    let mut image = match (icon.is_colored(), emphasis) {
        (_, Emphasis::Warn) => icon.mono(theme::WARN),
        (true, _) => icon.image(ui),
        (false, _) => icon.mono(Color32::WHITE),
    };
    if !enabled {
        image = image.tint(Color32::from_white_alpha(90));
    }
    let label = match emphasis {
        Emphasis::Normal => RichText::new(text),
        Emphasis::Primary => RichText::new(text).strong(),
        Emphasis::Warn => RichText::new(text).color(theme::WARN).strong(),
    };
    let mut b = if text.is_empty() {
        egui::Button::image(image)
    } else {
        egui::Button::image_and_text(image, label)
    }
    .image_tint_follows_text_color(!icon.is_colored() && emphasis != Emphasis::Warn)
    .min_size(egui::vec2(28.0, 28.0));
    b = match emphasis {
        Emphasis::Normal => b.frame_when_inactive(false),
        Emphasis::Primary => b,
        Emphasis::Warn => b.stroke(egui::Stroke::new(1.0, theme::WARN)),
    };
    let response = ui.add_enabled(enabled, b).on_hover_text(tip);
    match state {
        Err(reason) => response
            .on_disabled_hover_text(format!("{tip}\n{reason}"))
            .clicked(),
        Ok(()) => response.clicked(),
    }
}

fn connection_color(profile: &ConnectionProfile) -> Option<Color32> {
    if profile.production {
        Some(theme::DANGER)
    } else {
        profile.color.map(|[r, g, b]| Color32::from_rgb(r, g, b))
    }
}

/// A top-level menu of the menu bar. As in native menu bars, moving onto
/// another title while one of the bar's menus is open switches to it.
fn bar_menu(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    let response = ui.menu_button(title, add_contents).response;
    let ctx = ui.ctx();
    let popup_id = egui::Popup::default_response_id(&response);
    let open_key = ui.id().with("open_bar_menu");
    if response.hovered() && !egui::Popup::is_id_open(ctx, popup_id) {
        let open = ctx.data(|d| d.get_temp::<egui::Id>(open_key));
        if open.is_some_and(|id| egui::Popup::is_id_open(ctx, id)) {
            egui::Popup::open_id(ctx, popup_id);
            ctx.request_repaint();
        }
    }
    if egui::Popup::is_id_open(ctx, popup_id) {
        ctx.data_mut(|d| d.insert_temp(open_key, popup_id));
    }
}

/// Request id of requests that are answered with an id-carrying event.
fn request_key(request: &DbRequest) -> Option<u64> {
    match request {
        DbRequest::Execute { id, .. }
        | DbRequest::FetchMore { id, .. }
        | DbRequest::Meta { id, .. }
        | DbRequest::Task { id, .. }
        | DbRequest::Run { id, .. } => Some(*id),
        _ => None,
    }
}

fn event_id(event: &DbEvent) -> Option<u64> {
    match event {
        DbEvent::QueryStarted { id, .. }
        | DbEvent::Rows { id, .. }
        | DbEvent::Executed { id, .. }
        | DbEvent::Failed { id, .. }
        | DbEvent::Meta { id, .. }
        | DbEvent::TaskDone { id, .. } => Some(*id),
        _ => None,
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    /// eframe's storage, in memory.
    #[derive(Default)]
    pub(super) struct Memory(BTreeMap<String, String>);

    impl eframe::Storage for Memory {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }

        fn flush(&mut self) {}
    }

    /// Starts Pythia with what `storage` holds.
    pub(super) fn start(storage: &Memory) -> PythiaApp {
        let mut cc = eframe::CreationContext::_new_kittest(egui::Context::default());
        cc.storage = Some(storage);
        PythiaApp::new(&cc, None)
    }

    /// Favorites and recent objects survive a restart with no document
    /// open (#118).
    #[test]
    fn restored_without_documents() {
        let mut storage = Memory::default();
        let mut app = start(&storage);
        let object = ObjectRef {
            owner: "HR".into(),
            name: "EMPLOYEES".into(),
            object_type: pythia_db::ObjectType::Table,
        };
        app.browser.marks.touch("hr@db", object.clone());
        app.browser
            .marks
            .databases
            .insert("shop@mysql".into(), "shop".into());
        app.docs.clear();
        app.dock = DockState::new(vec![]);
        eframe::App::save(&mut app, &mut storage);

        let app = start(&storage);
        assert_eq!(app.browser.marks.recent.get("hr@db"), Some(&vec![object]));
        // The database last picked in the browser (#133).
        assert_eq!(app.browser.marks.databases["shop@mysql"], "shop");
        assert_eq!(app.docs.len(), 1, "a new SQL window");
    }

    /// Pythia starts with the connect dialog, on the profile used last
    /// (#132).
    #[test]
    fn starts_with_connect_dialog() {
        let mut storage = Memory::default();
        let mut app = start(&storage);
        assert!(app.connect_dialog.open, "first start");
        assert_eq!(app.connect_dialog.test_editing(), None);

        app.last_profile = Some(ConnectionProfile {
            name: "Acme test".into(),
            username: "hr".into(),
            connect_string: "acme:1521/test".into(),
            ..Default::default()
        });
        eframe::App::save(&mut app, &mut storage);

        let app = start(&storage);
        assert!(app.connect_dialog.open);
        assert_eq!(app.connect_dialog.test_editing(), Some("Acme test"));
    }

    /// Breakpoints, their conditions, watches and Break on exceptions are
    /// there again after a restart (#117).
    #[test]
    fn debugger_state_is_kept() {
        let mut storage = Memory::default();
        let mut app = start(&storage);
        let unit = pythia_debugger::Unit::new("HR", "API", pythia_debugger::UnitType::PackageBody);
        let at = |line| pythia_debugger::Breakpoint {
            unit: unit.clone(),
            line,
        };
        app.debug.toggle_breakpoint(unit.clone(), 12);
        app.debug.toggle_breakpoint(unit.clone(), 40);
        app.debug.set_enabled(&at(40), false);
        let options = pythia_debugger::BreakpointOptions {
            condition: Some("l_total > 3".into()),
            hit_count: Some(2),
        };
        app.debug.set_options(&at(12), options.clone());
        app.debug.add_watch("g_count");
        app.debug.set_break_on_exception(true);
        eframe::App::save(&mut app, &mut storage);

        let app = start(&storage);
        let debug = &app.debug;
        assert_eq!(debug.breakpoints, [at(12), at(40)].into());
        assert_eq!(debug.disabled, [at(40)].into());
        assert_eq!(debug.options.get(&at(12)), Some(&options));
        assert_eq!(debug.watches, ["g_count"]);
        assert!(debug.break_on_exception);
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn formats_editor_text() {
        let mut editor = crate::editor::Editor::new("SELECT a,b FROM t WHERE x=1");
        let message = format_editor(&mut editor, &pythia_plsql::FormatOptions::default());
        assert_eq!(message, "Formatted");
        assert_eq!(editor.text(), "select a, b\nfrom t\nwhere x = 1");
        assert_eq!(
            format_editor(&mut editor, &pythia_plsql::FormatOptions::default()),
            "Already formatted"
        );
    }
}

#[cfg(test)]
mod rename_tests {
    use super::rename_symbol;
    use crate::editor::Editor;

    #[test]
    fn renames_every_occurrence() {
        let src = "declare\n  l_total number := 0;\nbegin\n  l_total := l_total + 1;\n  dbms_output.put_line(l_total);\nend;";
        let mut e = Editor::new(src);
        e.select(src.find("l_total :=").unwrap()..src.find("l_total :=").unwrap());
        let message = rename_symbol(&mut e).unwrap();
        assert!(
            message.starts_with("Renaming 4 occurrences of l_total"),
            "{message}"
        );
        e.inner().insert_at_cursor("l_sum");
        let text = e.inner().text();
        assert_eq!(text.matches("l_sum").count(), 4, "{text}");
        assert!(!text.contains("l_total"));

        let mut e = Editor::new("select 1 from dual");
        assert!(
            rename_symbol(&mut e)
                .unwrap()
                .starts_with("Rename works for")
        );
    }
}

/// How completion writes and compares a database's names.
fn name_style(dialect: pythia_db::Dialect) -> pythia_lsp_core::NameStyle {
    match dialect.kind() {
        pythia_db::DbKind::Oracle => pythia_lsp_core::NameStyle::Upper,
        pythia_db::DbKind::Postgres => pythia_lsp_core::NameStyle::Lower,
        pythia_db::DbKind::MySql => pythia_lsp_core::NameStyle::Backticks,
        pythia_db::DbKind::SqlServer => pythia_lsp_core::NameStyle::Brackets,
    }
}
