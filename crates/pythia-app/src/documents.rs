//! Documents are the dockable windows bound to a connection: SQL windows,
//! program windows and object views.

use std::sync::atomic::{AtomicU64, Ordering};

use egui::{RichText, Ui};
use pythia_db::{DbEvent, DbRequest, ObjectType};
use serde::{Deserialize, Serialize};

use crate::object_view::ObjectView;
use crate::program_window::ProgramWindow;
use crate::sql_window::SqlWindow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DocId(pub u64);

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// Unique id for a database request, across all sessions.
pub fn request_id() -> u64 {
    NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
}

/// Where a document wants a request to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The document's own session (its transaction).
    Own,
    /// The connection's main session (metadata lookups).
    Main,
    /// The connection's metadata session (loading object views and forms).
    Meta,
    /// The document's own session on another connection (compare tools).
    Connection(String),
    /// The document's own session, a statement of a SQL window's script
    /// that the session runs without waiting for the window (#291).
    Script(pythia_db::Script),
}

/// A stored object, as documents and the browser refer to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectRef {
    pub owner: String,
    pub name: String,
    pub object_type: ObjectType,
}

impl ObjectRef {
    /// `owner.name`, or just the name of a user or role (no owner).
    pub fn qualified(&self) -> String {
        if self.owner.is_empty() {
            return quote_ident(&self.name);
        }
        format!("{}.{}", quote_ident(&self.owner), quote_ident(&self.name))
    }
}

/// Quotes an identifier only if Oracle would need it.
pub fn quote_ident(name: &str) -> String {
    pythia_db::Dialect::ORACLE.quote_ident(name)
}

/// Things a document asks the app to do after a frame or an event.
#[derive(Default)]
pub struct Effects {
    pub requests: Vec<(Target, DbRequest)>,
    pub open: Vec<OpenRequest>,
    /// Statements that ran successfully, for the query history.
    pub executed: Vec<String>,
    /// The document wants its running statement cancelled.
    pub cancel: bool,
    /// Lines for the application log.
    pub log: Vec<String>,
    /// Dictionary data code assistance is waiting for.
    pub catalog_requests: Vec<pythia_lsp_core::CatalogRequest>,
    /// Run this anonymous block under the debugger.
    pub debug: Option<String>,
    /// Breakpoints toggled in an editor: (unit, line).
    pub breakpoints: Vec<(pythia_debugger::Unit, u32)>,
    /// Object actions chosen in an editor, for the object browser to carry
    /// out on this connection.
    pub object_actions: Vec<(String, crate::browser::ObjectAction)>,
    /// App commands to run (e.g. Format from an editor's context menu).
    pub commands: Vec<crate::commands::Command>,
    /// Recompile invalid objects on the document's session.
    pub recompile: Option<crate::invalid_window::RecompileRequest>,
    /// Objects were compiled or dropped: the invalid objects count is out
    /// of date.
    pub invalid_changed: bool,
    /// Objects may have changed, or were read again: completion forgets the
    /// connection's names (#198).
    pub catalog_changed: bool,
    /// An object was dropped: its views close (#200).
    pub dropped: Option<ObjectRef>,
    /// The editor jumped (go to definition, …) from this place: (part,
    /// byte offset), for going back.
    pub jumped_from: Option<(usize, usize)>,
    /// References found in the document, to list.
    pub references: Option<Vec<Reference>>,
    /// Requests for the AI assistant (e.g. slow SQL in the monitor).
    pub ai: Vec<crate::ai::AiRequest>,
    /// An object was created or changed: the object browser shows it as
    /// it is now.
    pub changed: Option<ObjectRef>,
    /// The document is done (e.g. a form that handed over to another
    /// window) and closes.
    pub close: bool,
    /// A database was picked in the object browser (MySQL): the
    /// connection's sessions use it.
    pub database: Option<String>,
}

/// A found reference in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Part of a program window (0 elsewhere).
    pub part: usize,
    pub range: std::ops::Range<usize>,
    /// 1-based line and its text.
    pub line: usize,
    pub text: String,
    /// "Spec", "Body", or none for a single text.
    pub part_label: Option<&'static str>,
    /// The name referred to.
    pub word: String,
}

impl Reference {
    pub fn new(
        part: usize,
        range: std::ops::Range<usize>,
        source: &str,
        part_label: Option<&'static str>,
    ) -> Self {
        let start = range.start.min(source.len());
        let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = source[start..]
            .find('\n')
            .map_or(source.len(), |i| start + i);
        Self {
            part,
            line: source[..start].matches('\n').count() + 1,
            text: source[line_start..line_end].trim().to_owned(),
            word: source.get(range.clone()).unwrap_or_default().to_owned(),
            range,
            part_label,
        }
    }
}

impl Effects {
    pub fn request(&mut self, target: Target, request: DbRequest) {
        self.requests.push((target, request));
    }

    pub fn extend(&mut self, other: Effects) {
        self.requests.extend(other.requests);
        self.open.extend(other.open);
        self.executed.extend(other.executed);
        self.cancel |= other.cancel;
        self.log.extend(other.log);
        self.catalog_requests.extend(other.catalog_requests);
        self.debug = other.debug.or(self.debug.take());
        self.breakpoints.extend(other.breakpoints);
        self.object_actions.extend(other.object_actions);
        self.commands.extend(other.commands);
        self.jumped_from = other.jumped_from.or(self.jumped_from.take());
        self.recompile = other.recompile.or(self.recompile.take());
        self.invalid_changed |= other.invalid_changed;
        self.catalog_changed |= other.catalog_changed;
        self.dropped = other.dropped.or(self.dropped.take());
        self.references = other.references.or(self.references.take());
        self.ai.extend(other.ai);
        self.changed = other.changed.or(self.changed.take());
        self.close |= other.close;
        self.database = other.database.or(self.database.take());
    }
}

/// Request to open a new document (from the browser or another document).
#[derive(Debug, Clone)]
pub enum OpenRequest {
    /// Open a file in a SQL window.
    File {
        path: std::path::PathBuf,
        connection: Option<String>,
    },
    Sql {
        connection: String,
        text: String,
        run: bool,
    },
    /// A SQL window with a block that calls a subprogram, debugged right
    /// away; the run stops on entry to `entry` (unit and subprogram).
    Debug {
        connection: String,
        text: String,
        entry: (pythia_debugger::Unit, String),
    },
    Program {
        connection: String,
        object: ObjectRef,
    },
    Object {
        connection: String,
        object: ObjectRef,
    },
    /// Open a form editor (sequence, view, synonym, index, grants).
    EditObject {
        connection: String,
        target: crate::object_editor::EditTarget,
    },
    /// Edit a table's structure, starting on a tab.
    EditTable {
        connection: String,
        object: ObjectRef,
        tab: crate::table_editor::TableTab,
    },
    /// Build a new table in `owner`, empty or as a copy of `like`.
    NewTable {
        connection: String,
        owner: String,
        like: Option<ObjectRef>,
    },
    /// A new program unit, not compiled yet: a program window with these
    /// sources (spec first).
    NewProgram {
        connection: String,
        object: ObjectRef,
        sources: Vec<String>,
    },
    /// Open a program window and put the cursor on a 1-based line.
    ProgramAt {
        connection: String,
        object: ObjectRef,
        line: usize,
    },
    /// Open a package (or type) at a subprogram: its implementation in the
    /// body, or its declaration when there's no body.
    ProgramMember {
        connection: String,
        object: ObjectRef,
        member: String,
        /// Select this parameter of the member (named notation).
        parameter: Option<String>,
    },
    /// The maintenance window of a table, materialized view or index, on
    /// a task (a dashboard finding's fix) or the first one.
    Maintenance {
        connection: String,
        object: ObjectRef,
        task: Option<(crate::maintenance::Task, crate::maintenance::Options)>,
    },
    /// The maintenance scan of a schema (Oracle, MySQL) or the database.
    MaintenanceScan { connection: String, schema: String },
    /// The invalid objects window of a schema (or all user schemas),
    /// recompiling everything right away if asked.
    InvalidObjects {
        connection: String,
        schema: Option<String>,
        recompile: bool,
    },
    /// Insert text at the cursor of the focused SQL window (or a new one).
    InsertText { text: String },
    /// Open the connection manager.
    ConnectDialog,
    /// Show this connection in the object browser.
    Browse { connection: String },
    Monitor {
        connection: String,
        start: crate::monitor_window::MonitorStart,
    },
}

/// A query result as shown in a grid.
pub type TableData = (Vec<pythia_db::Column>, Vec<Vec<pythia_db::Cell>>);

#[derive(Serialize, Deserialize)]
// A handful of documents, never copied: variant sizes don't matter.
#[allow(clippy::large_enum_variant)]
pub enum DocKind {
    Sql(SqlWindow),
    Program(ProgramWindow),
    Object(ObjectView),
    Monitor(crate::monitor_window::MonitorWindow),
    Compare(crate::compare_window::CompareWindow),
    Search(crate::source_search::SourceSearch),
    Import(crate::import_window::ImportWindow),
    DataSearch(crate::data_search::DataSearch),
    TableEditor(crate::table_editor::TableEditor),
    Jobs(crate::jobs_window::JobsWindow),
    Diff(crate::text_diff::TextDiff),
    ExportTables(crate::export_tables::ExportTables),
    ObjectEditor(crate::object_editor::ObjectEditor),
    Invalid(crate::invalid_window::InvalidObjectsWindow),
    Maintenance(crate::maintenance::MaintenanceWindow),
    MaintenanceScan(crate::maintenance::ScanWindow),
}

#[derive(Serialize, Deserialize)]
pub struct Document {
    /// Display name of the connection this document runs on.
    pub connection: Option<String>,
    pub kind: DocKind,
}

/// Context handed to documents while drawing.
pub struct DocContext<'a> {
    pub id: DocId,
    pub settings: &'a pythia_config::Settings,
    pub connected: bool,
    pub fetch_size: usize,
    /// Display name of the document's connection.
    pub connection: Option<String>,
    /// Dictionary cache of the connection, for code assistance.
    pub catalog: Option<&'a pythia_lsp_core::CatalogCache>,
    /// Bumped whenever new dictionary data arrived.
    pub catalog_revision: u64,
    /// Names of all open connections.
    pub connections: &'a [String],
    /// Debugger state, for breakpoint and execution markers.
    pub debug: Option<&'a pythia_debugger::ui::DebugSession>,
    /// Favorites and recent objects, for the editors' context menu.
    pub marks: Option<&'a crate::browser::Marks>,
    /// How the document's connection is logged in (SYSDBA, …).
    pub privilege: Option<pythia_config::Privilege>,
    /// The document connection's database: what it can offer.
    pub dialect: pythia_db::Dialect,
    /// The connection's server is MariaDB, which replaces routines and
    /// triggers in place where MySQL can't.
    pub mariadb: bool,
    /// The connection's profile is read-only: nothing that changes data is
    /// offered.
    pub read_only: bool,
    /// The connection's profile is marked as production: ask before what
    /// can't be undone.
    pub production: bool,
}

impl Document {
    pub fn title(&self) -> String {
        match &self.kind {
            DocKind::Sql(w) => w.title.clone(),
            DocKind::Program(w) => w.title(),
            DocKind::Object(v) => v.title(),
            DocKind::Monitor(m) => m.title(),
            DocKind::Compare(c) => c.title(),
            DocKind::Search(s) => s.title(),
            DocKind::Import(i) => i.title(),
            DocKind::DataSearch(d) => d.title(),
            DocKind::TableEditor(t) => t.title(),
            DocKind::Jobs(j) => j.title(),
            DocKind::Diff(d) => d.title(),
            DocKind::ExportTables(e) => e.title(),
            DocKind::ObjectEditor(e) => e.title(),
            DocKind::Invalid(i) => i.title(),
            DocKind::Maintenance(m) => m.title(),
            DocKind::MaintenanceScan(m) => m.title(),
        }
    }

    /// The tab's title; `elsewhere` names its connection when that isn't
    /// the active one.
    pub fn tab_title(&self, color: Option<egui::Color32>, elsewhere: Option<&str>) -> RichText {
        let busy = if self.is_busy() { " …" } else { "" };
        let dirty = if self.is_dirty() { " •" } else { "" };
        let on = elsewhere.map(|c| format!(" · {c}")).unwrap_or_default();
        let text = RichText::new(format!("{}{on}{dirty}{busy}", self.title()));
        match color {
            Some(c) => text.color(c),
            None => text,
        }
    }

    pub fn is_busy(&self) -> bool {
        match &self.kind {
            DocKind::Sql(w) => w.is_running(),
            DocKind::Program(w) => w.is_busy(),
            DocKind::Object(v) => v.is_loading(),
            DocKind::Monitor(m) => m.is_busy(),
            DocKind::Compare(c) => c.is_busy(),
            DocKind::Search(s) => s.is_busy(),
            DocKind::Import(i) => i.is_busy(),
            DocKind::DataSearch(d) => d.is_busy(),
            DocKind::TableEditor(t) => t.is_busy(),
            DocKind::Jobs(j) => j.is_busy(),
            DocKind::Diff(_) => false,
            DocKind::ExportTables(e) => e.is_busy(),
            DocKind::ObjectEditor(e) => e.is_busy(),
            DocKind::Invalid(i) => i.is_busy(),
            DocKind::Maintenance(m) => m.is_busy(),
            DocKind::MaintenanceScan(m) => m.is_busy(),
        }
    }

    /// A SQL window's result changes that weren't applied.
    pub fn has_unapplied_edits(&self) -> bool {
        match &self.kind {
            DocKind::Sql(w) => w.has_unapplied_edits(),
            _ => false,
        }
    }

    pub fn is_dirty(&self) -> bool {
        match &self.kind {
            DocKind::Program(w) => w.is_dirty(),
            DocKind::Sql(w) => w.is_dirty(),
            DocKind::TableEditor(t) => t.is_dirty(),
            DocKind::ObjectEditor(e) => e.is_dirty(),
            _ => false,
        }
    }

    /// Asks the editor for navigation at its caret (SQL and program windows).
    pub fn navigate(&mut self, request: crate::assist::Request) {
        match &mut self.kind {
            DocKind::Sql(w) => w.navigate(request),
            DocKind::Program(w) => w.navigate(request),
            _ => {}
        }
    }

    /// The editor part and caret, for navigation history.
    pub fn position(&self) -> Option<(usize, usize)> {
        match &self.kind {
            DocKind::Sql(w) => Some((0, w.editor.cursor())),
            DocKind::Program(w) => w.position(),
            _ => None,
        }
    }

    /// Shows a range of an editor part.
    pub fn goto_position(&mut self, part: usize, range: std::ops::Range<usize>) {
        match &mut self.kind {
            DocKind::Sql(w) => w.editor.select(range),
            DocKind::Program(w) => w.goto_position(part, range),
            _ => {}
        }
    }

    /// Declarations to jump to with Go to Symbol.
    pub fn symbols(&self) -> Vec<crate::dialogs::SymbolItem> {
        match &self.kind {
            DocKind::Sql(w) => w.symbols(),
            DocKind::Program(w) => w.symbols(),
            _ => Vec::new(),
        }
    }

    /// Text that "execute" and friends act on, if this document has an editor.
    pub fn sql_window(&mut self) -> Option<&mut SqlWindow> {
        match &mut self.kind {
            DocKind::Sql(w) => Some(w),
            _ => None,
        }
    }

    pub fn on_event(&mut self, event: DbEvent, cx: &DocContext) -> Effects {
        match &mut self.kind {
            DocKind::Sql(w) => w.on_event(event, cx),
            DocKind::Program(w) => w.on_event(event),
            DocKind::Object(v) => v.on_event(event),
            DocKind::Monitor(m) => m.on_event(event),
            DocKind::Compare(c) => c.on_event(event),
            DocKind::Search(s) => s.on_event(event),
            DocKind::Import(i) => i.on_event(event),
            DocKind::DataSearch(d) => d.on_event(event),
            DocKind::TableEditor(t) => t.on_event(event),
            DocKind::Jobs(j) => j.on_event(event),
            DocKind::Diff(_) => Effects::default(),
            DocKind::ExportTables(e) => e.on_event(event),
            DocKind::ObjectEditor(e) => e.on_event(event),
            DocKind::Invalid(i) => i.on_event(event),
            DocKind::Maintenance(m) => m.on_event(event, cx.dialect),
            DocKind::MaintenanceScan(m) => m.on_event(event, cx.dialect),
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        match &mut self.kind {
            DocKind::Sql(w) => w.ui(ui, cx),
            DocKind::Program(w) => w.ui(ui, cx),
            DocKind::Object(v) => v.ui(ui, cx, self.connection.as_deref()),
            DocKind::Monitor(m) => m.ui(ui, cx),
            DocKind::Compare(c) => c.ui(ui, cx),
            DocKind::Search(s) => s.ui(ui, cx),
            DocKind::Import(i) => i.ui(ui, cx),
            DocKind::DataSearch(d) => d.ui(ui, cx),
            DocKind::TableEditor(t) => t.ui(ui, cx),
            DocKind::Jobs(j) => j.ui(ui, cx),
            DocKind::Diff(d) => d.ui(ui, cx),
            DocKind::ExportTables(e) => e.ui(ui, cx),
            DocKind::ObjectEditor(e) => e.ui(ui, cx),
            DocKind::Invalid(i) => i.ui(ui, cx),
            DocKind::Maintenance(m) => m.ui(ui, cx),
            DocKind::MaintenanceScan(m) => m.ui(ui, cx),
        }
    }

    /// Called once after the document was created or its connection changed;
    /// `dialect` is the connection's database.
    pub fn on_attach(&mut self, dialect: pythia_db::Dialect) -> Effects {
        match &mut self.kind {
            DocKind::Sql(_) => Effects::default(),
            DocKind::Program(w) => w.load(),
            DocKind::Object(v) => v.load(),
            DocKind::TableEditor(t) => t.load(),
            DocKind::ObjectEditor(e) => e.attach(dialect),
            _ => Effects::default(),
        }
    }

    /// Line/column of the cursor and a status summary for the status bar.
    pub fn status(&self) -> (Option<(usize, usize)>, Option<String>) {
        match &self.kind {
            DocKind::Sql(w) => (Some(w.state.line_col), w.result_status()),
            DocKind::Program(w) => (Some(w.line_col()), None),
            _ => (None, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        assert_eq!(quote_ident("EMP"), "EMP");
        assert_eq!(quote_ident("EMP_2$"), "EMP_2$");
        assert_eq!(quote_ident("Emp"), "\"Emp\"");
        assert_eq!(quote_ident("TABLE"), "\"TABLE\"");
        assert_eq!(quote_ident("A B"), "\"A B\"");
    }
}
