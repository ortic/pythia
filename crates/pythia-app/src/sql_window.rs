//! SQL window: editor on top, results / plan / messages / DBMS_OUTPUT below.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use std::time::Duration;

use egui::{RichText, Ui};
use pythia_db::{Cell, Column, DbError, DbEvent, DbRequest, Feature, MetaQuery, MetaResult};
use pythia_plsql::{
    StatementKind, Syntax, TokenKind, split_statements_in, statement_at_in, tokenize,
};
use pythia_ui::icons::{Icon, icon_button, icon_text_button};
use pythia_ui::{GridState, ResultGrid, selection_data, shown_data, to_tsv};
use serde::{Deserialize, Serialize};

use crate::documents::{DocContext, Effects, ObjectRef, OpenRequest, Target, request_id};
use crate::editor::{Editor, Marker, MarkerKind};

#[derive(Serialize, Deserialize)]
pub struct SqlWindow {
    pub title: String,
    pub editor: Editor,
    /// Last values entered for bind variables, by lowercase name.
    #[serde(default)]
    pub bind_values: BTreeMap<String, Option<String>>,
    /// `&&var` / `DEFINE` values, by lowercase name.
    #[serde(default)]
    pub defines: BTreeMap<String, String>,
    #[serde(default = "yes")]
    pub substitution: bool,
    /// The Variables panel shows while the text has variables (#110).
    #[serde(default = "yes")]
    pub show_variables: bool,
    /// File the window was opened from or saved to.
    #[serde(default)]
    pub path: Option<std::path::PathBuf>,
    /// Text as last saved to `path`, to detect unsaved changes.
    #[serde(default)]
    pub saved_text: String,
    /// How `path` is written back.
    #[serde(default)]
    pub encoding: crate::text_file::Encoding,
    #[serde(default)]
    pub line_ending: crate::text_file::LineEnding,
    #[serde(skip)]
    pub state: RunState,
    #[serde(skip)]
    assist: crate::assist::Assist,
}

fn yes() -> bool {
    true
}

/// Text revision, cursor and selection of the editor.
type EditorPosition = (u64, usize, Option<Range<usize>>);

/// Everything that is not persisted with the layout.
#[derive(Default)]
pub struct RunState {
    /// Statements still to run in the current script execution.
    pub queue: VecDeque<PendingStatement>,
    /// Statements in this run and how many finished (for progress).
    pub total: usize,
    pub done: usize,
    /// `WHENEVER SQLERROR CONTINUE`: keep going after errors.
    pub continue_on_error: bool,
    /// The statement currently running.
    pub running: Option<Running>,
    /// Request id of a "fetch more" in flight.
    pub fetching: Option<u64>,
    pub results: Vec<QueryResult>,
    /// Foreign key lookups in flight: task id → result id.
    pub relation_requests: BTreeMap<u64, u64>,
    /// Task id of the key lookup an edit waits for.
    pub key_lookup: Option<u64>,
    /// "Execute to file": the file dialog (with the query), then the run.
    pub to_file_pick: Option<(
        std::sync::mpsc::Receiver<Option<std::path::PathBuf>>,
        String,
    )>,
    pub to_file: Option<crate::to_file::ToFile>,
    pub plan: Option<Plan>,
    /// "Run with statistics": request id and report.
    pub statistics: Option<(u64, Option<Result<crate::statistics::Report, String>>)>,
    /// Its plan as a tree, and whether DBMS_XPLAN's text shows instead.
    pub statistics_view: pythia_ui::plan::PlanView,
    pub statistics_text: bool,
    /// "Profile": request id and the DBMS_HPROF profile.
    pub profile: Option<(u64, Option<Result<crate::profiler::Profile, String>>)>,
    pub messages: Vec<Message>,
    pub output: Vec<String>,
    pub bottom: BottomTab,
    pub line_col: (usize, usize),
    /// A statement waiting for values in the Variables panel.
    pub prompt: Option<VariablePrompt>,
    /// The waiting statement is for a debug run.
    pub prompt_for_debug: bool,
    /// Variables of the text, and the text revision, substitution setting
    /// and syntax they were found with.
    variables: Option<((u64, bool, Syntax), Vec<Variable>)>,
    /// Editor line where the debugged block starts, while this window's
    /// block is the one being debugged.
    pub debug_start: Option<usize>,
    /// The subprogram the window was opened to debug (Debug…): its debug
    /// runs stop on entry to it.
    pub debug_entry: Option<(pythia_debugger::Unit, String)>,
    /// Whether Debug has a block to run, and the editor position that was
    /// worked out for.
    debuggable: Option<(EditorPosition, bool)>,
    /// Result of a running export (file dialog + write on a helper thread).
    pub export: Option<std::sync::mpsc::Receiver<Result<Option<std::path::PathBuf>, String>>>,
    pub selection_summary: Option<String>,
    /// Where the connection's scripts end their statements (from the
    /// document context, each frame).
    pub syntax: Syntax,
    /// Error stacks to look up, not sent yet.
    stack_lookups: Vec<StackLookup>,
}

/// A task id and the `(owner, name, line)` of each stored unit in an error's
/// stack.
type StackLookup = (u64, Vec<(String, String, u32)>);

#[derive(Debug, Clone)]
pub struct PendingStatement {
    pub kind: StatementKind,
    pub sql: String,
    /// Byte offset of the statement in the editor text.
    pub start: usize,
    /// Statements generated by Pythia itself run verbatim.
    pub internal: Option<Internal>,
    /// Script file the statement came from (`@file`); `None` for the editor.
    pub origin: Option<std::path::PathBuf>,
}

/// Statements Pythia generates for data editing.
#[derive(Debug, Clone)]
pub enum Internal {
    /// Looking up the key of `table` (MySQL) before the query that makes
    /// result `replaces` editable.
    EditKey {
        table: String,
        source: String,
        replaces: u64,
    },
    /// Query fetching what identifies the rows (ROWIDs, key columns); its
    /// result becomes editable.
    EditQuery {
        edit: crate::data_edit::Editable,
        /// Result (by request id) this query refreshes in place.
        replaces: Option<u64>,
    },
    /// DML applying grid changes; must affect exactly `expect_rows` rows.
    Dml { expect_rows: u64, what: String },
}

pub struct Running {
    pub start: usize,
    pub internal: Option<Internal>,
    pub sql: String,
}

pub struct QueryResult {
    pub id: u64,
    pub title: String,
    pub sql: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    /// More rows can be fetched.
    pub more: bool,
    pub elapsed: Duration,
    pub grid: GridState,
    /// Set once the result was made editable.
    pub edit: Option<crate::data_edit::Editable>,
    /// Quick filter text.
    pub filter: String,
    /// Shown as a chart instead of the grid.
    pub chart: Option<crate::chart_view::ChartState>,
    /// Foreign keys of the queried table and, per grid link, its relation.
    pub relations: Vec<crate::relations::Relation>,
    pub link_relations: Vec<usize>,
}

pub struct Plan {
    pub id: u64,
    pub view: pythia_ui::plan::PlanView,
    pub result: Option<Result<Vec<pythia_ui::plan::PlanRow>, String>>,
}

pub struct Message {
    pub level: Level,
    pub text: String,
    /// Clicking the message selects this range in the editor.
    pub jump: Option<Range<usize>>,
    /// The lines of an Oracle error's PL/SQL stack that lead to code.
    pub stack: Vec<Frame>,
}

/// An `ORA-06512` line of an error, and the code it points at (#112).
pub struct Frame {
    /// Index of the frame's line in the message text.
    pub text_line: usize,
    pub target: FrameTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FrameTarget {
    /// The task looking up the stored unit's code.
    Pending(u64),
    /// A line of a stored unit's part, and its text.
    Program {
        object: ObjectRef,
        line: usize,
        text: String,
    },
    /// A line of the statement that ran, as a range in the editor, and its
    /// text.
    Statement { range: Range<usize>, text: String },
    /// Why the code can't be shown.
    Hidden(String),
}

impl FrameTarget {
    /// The code can be shown.
    pub fn opens(&self) -> bool {
        matches!(self, Self::Program { .. } | Self::Statement { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BottomTab {
    Result(usize),
    Plan,
    Statistics,
    Profile,
    #[default]
    Messages,
    Output,
}

/// A result toolbar's layout, remembered between frames: whether its buttons
/// show icons only, and how wide they are with their labels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct ResultBar {
    compact: bool,
    full_width: f32,
}

impl ResultBar {
    /// Width the filter and the row count need at least, left of the buttons.
    const MIN_LEFT: f32 = 190.0;
}

/// A statement that waits for variables without a value.
pub struct VariablePrompt {
    statement: PendingStatement,
    /// Focus the first variable without a value (once).
    focus: bool,
}

/// A variable the Variables panel lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Variable {
    /// `:name`, as first written; values are kept by lowercase name.
    Bind(String),
    /// `&name` / `&&name`, lowercase.
    Substitution(String),
}

impl Variable {
    fn label(&self) -> String {
        match self {
            Variable::Bind(name) => format!(":{name}"),
            Variable::Substitution(name) => format!("&{name}"),
        }
    }
}

impl SqlWindow {
    pub fn new(title: String, text: String) -> Self {
        Self {
            title,
            editor: Editor::new(text),
            bind_values: BTreeMap::new(),
            defines: BTreeMap::new(),
            substitution: true,
            show_variables: true,
            path: None,
            saved_text: String::new(),
            encoding: Default::default(),
            line_ending: Default::default(),
            state: RunState::default(),
            assist: Default::default(),
        }
    }

    /// Whether the completion popup is open and offers `label` (tests).
    #[cfg(test)]
    pub fn completion_visible_with(&self, label: &str) -> bool {
        self.editor
            .completion_labels()
            .iter()
            .any(|l| l.eq_ignore_ascii_case(label))
    }

    /// A window loaded from a file.
    pub fn from_file(path: std::path::PathBuf, file: crate::text_file::TextFile) -> Self {
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut w = Self::new(title, file.text.clone());
        w.path = Some(path);
        w.saved_text = file.text;
        w.encoding = file.encoding;
        w.line_ending = file.line_ending;
        w
    }

    /// Unsaved changes to a file-backed window.
    pub fn is_dirty(&self) -> bool {
        self.path.is_some() && self.editor.text() != self.saved_text
    }

    /// Writes the text to `path` and makes it the window's file.
    pub fn save_to(&mut self, path: std::path::PathBuf) -> std::io::Result<()> {
        crate::text_file::write(&path, self.editor.text(), self.encoding, self.line_ending)?;
        self.saved_text = self.editor.text().to_owned();
        self.title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.path = Some(path);
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.state.running.is_some()
            || self.state.to_file.is_some()
            || self.state.fetching.is_some()
            || self.state.prompt.is_some()
            || self
                .state
                .statistics
                .as_ref()
                .is_some_and(|(_, r)| r.is_none())
            || self
                .state
                .profile
                .as_ref()
                .is_some_and(|(_, r)| r.is_none())
    }

    /// Whether Debug has an anonymous block to run (the selection's first
    /// statement or the one under the cursor), as of the last frame.
    pub fn debuggable(&self) -> bool {
        self.state.debuggable.as_ref().is_some_and(|(_, ok)| *ok)
    }

    /// Works out [`Self::debuggable`] again once the text, cursor or
    /// selection changed; splitting the script every frame would be slow.
    /// Returns true if the answer changed.
    fn refresh_debuggable(&mut self) -> bool {
        let key = (
            self.editor.revision(),
            self.editor.cursor(),
            self.editor.selection(),
        );
        if self
            .state
            .debuggable
            .as_ref()
            .is_some_and(|(k, _)| *k == key)
        {
            return false;
        }
        let ok = self.statements(true).first().is_some_and(is_debuggable);
        let changed = ok != self.debuggable();
        self.state.debuggable = Some((key, ok));
        changed
    }

    pub fn statements(&self, current_only: bool) -> Vec<PendingStatement> {
        let text = self.editor.text();
        let selection = self.editor.selection();
        let (base, src) = match &selection {
            Some(sel) => (sel.start, &text[sel.clone()]),
            None => (0, text),
        };
        let statements = if current_only && selection.is_none() {
            statement_at_in(src, self.editor.cursor(), self.state.syntax)
                .into_iter()
                .collect()
        } else {
            split_statements_in(src, self.state.syntax)
        };
        statements
            .into_iter()
            .map(|s| PendingStatement {
                origin: None,
                internal: None,
                kind: s.kind,
                sql: s.text(src).to_owned(),
                start: base + s.range.start,
            })
            .collect()
    }

    /// Starts executing the selection / whole text, or the current statement.
    pub fn execute(&mut self, current_only: bool, cx: &DocContext) -> Effects {
        // Executing again runs the statement waiting for values.
        if self.waiting_for_values() {
            return self.run_prompted(cx);
        }
        if self.is_running() {
            return Effects::default();
        }
        self.state.syntax = cx.dialect.syntax();
        let statements = self.statements(current_only);
        let state = &mut self.state;
        state.results.clear();
        state.plan = None;
        state.messages.clear();
        state.output.clear();
        state.bottom = BottomTab::Messages;
        self.editor.set_markers(Vec::new());
        if statements.is_empty() {
            self.info("Nothing to execute");
            return Effects::default();
        }
        self.state.total = statements.len();
        self.state.done = 0;
        self.state.continue_on_error = false;
        self.state.queue = statements.into();
        self.run_next(cx)
    }

    /// Asks the editor for navigation at its caret.
    pub fn navigate(&mut self, request: crate::assist::Request) {
        let at = self.editor.cursor();
        self.assist.request(request, at);
    }

    /// Declarations in the text, for Go to Symbol.
    pub fn symbols(&self) -> Vec<crate::dialogs::SymbolItem> {
        crate::dialogs::symbol_items(0, self.editor.text(), None)
    }

    /// 1-based editor line of a byte offset.
    fn line_of(&self, offset: usize) -> usize {
        let text = self.editor.text();
        text[..offset.min(text.len())].matches('\n').count() + 1
    }

    /// Debugs the PL/SQL block under the cursor (asks for variables first).
    pub fn debug_current(&mut self) -> Effects {
        let mut effects = Effects::default();
        if self.is_running() {
            return effects;
        }
        let Some(stmt) = self.statements(true).into_iter().next() else {
            return effects;
        };
        if !is_debuggable(&stmt) {
            self.warn(
                "Debugging runs an anonymous block (BEGIN … END;). Debug… on a procedure or function creates one and debugs it.",
            );
            self.state.bottom = BottomTab::Messages;
            return effects;
        }
        if self.needs_values(&stmt.sql) {
            self.wait_for_values(stmt, true);
            return effects;
        }
        let (sql, binds) = self.with_values(&stmt.sql);
        self.state.debug_start = Some(self.line_of(stmt.start));
        effects.debug = Some(inline_binds(&sql, &binds));
        effects
    }

    /// Runs the current query fetching all rows, then shows the actual plan
    /// and session statistics.
    pub fn execute_with_statistics(&mut self) -> Effects {
        let mut effects = Effects::default();
        if self.is_running() {
            return effects;
        }
        let Some(stmt) = self.statements(true).into_iter().next() else {
            return effects;
        };
        let first = pythia_plsql::tokenize(&stmt.sql)
            .into_iter()
            .find(|t| !t.kind.is_trivia())
            .map(|t| stmt.sql[t.span].to_ascii_uppercase());
        if !matches!(first.as_deref(), Some("SELECT" | "WITH")) {
            self.warn("Run with statistics works for queries");
            self.state.bottom = BottomTab::Messages;
            return effects;
        }
        if !find_binds(&stmt.sql).is_empty() {
            self.warn("Run with statistics doesn't support bind variables yet; use literals");
            self.state.bottom = BottomTab::Messages;
            return effects;
        }
        let sql = if self.substitution {
            substitute(&stmt.sql, &self.defines)
        } else {
            stmt.sql
        };
        let id = request_id();
        self.state.statistics = Some((id, None));
        self.state.bottom = BottomTab::Statistics;
        let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
            let report = crate::statistics::run(conn, &sql)?;
            Ok(Box::new(report) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    /// Runs the current query writing all rows to a file chosen by the user.
    pub fn execute_to_file(&mut self) {
        if self.is_running() || self.state.to_file_pick.is_some() {
            return;
        }
        let Some(stmt) = self.statements(true).into_iter().next() else {
            return;
        };
        let first = pythia_plsql::tokenize(&stmt.sql)
            .into_iter()
            .find(|t| !t.kind.is_trivia())
            .map(|t| stmt.sql[t.span].to_ascii_uppercase());
        if !matches!(first.as_deref(), Some("SELECT" | "WITH")) {
            self.warn("Execute to file works for queries");
            self.state.bottom = BottomTab::Messages;
            return;
        }
        if !find_binds(&stmt.sql).is_empty() {
            self.warn("Execute to file doesn't support bind variables yet; use literals");
            self.state.bottom = BottomTab::Messages;
            return;
        }
        let sql = if self.substitution {
            substitute(&stmt.sql, &self.defines)
        } else {
            stmt.sql
        };
        self.state.to_file_pick = Some((crate::to_file::pick_file(), sql));
    }

    /// Starts writing once the file was chosen; shows progress.
    fn poll_to_file(&mut self, ui: &mut Ui) -> Effects {
        let mut effects = Effects::default();
        if let Some((rx, sql)) = &self.state.to_file_pick {
            match rx.try_recv() {
                Ok(Some(path)) => {
                    let sql = sql.clone();
                    self.state.to_file_pick = None;
                    let id = request_id();
                    let rows = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let (task_sql, task_path, progress) = (sql.clone(), path.clone(), rows.clone());
                    let task: pythia_db::TaskFn =
                        Box::new(move |conn: &pythia_db::oracle::Connection| {
                            let n = crate::to_file::run(conn, &task_sql, &task_path, &progress)?;
                            Ok(Box::new(n) as Box<dyn std::any::Any + Send>)
                        });
                    self.state.to_file = Some(crate::to_file::ToFile {
                        id,
                        path,
                        sql,
                        rows,
                        started: std::time::Instant::now(),
                    });
                    effects.request(Target::Own, DbRequest::Task { id, task });
                }
                Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.state.to_file_pick = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
            }
        }
        if self.state.to_file.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
        effects
    }

    /// Stops the run: a read-only connection only runs queries.
    pub fn refuse_read_only(&mut self) {
        self.state.queue.clear();
        self.info("Connection is read-only: only queries are allowed");
        self.state.bottom = BottomTab::Messages;
    }

    /// Runs the PL/SQL block at the cursor under the hierarchical profiler.
    pub fn profile(&mut self, read_only: bool) -> Effects {
        let mut effects = Effects::default();
        if self.is_running() {
            return effects;
        }
        if read_only {
            self.refuse_read_only();
            return effects;
        }
        let Some(stmt) = self.statements(true).into_iter().next() else {
            return effects;
        };
        let sql = if self.substitution {
            substitute(&stmt.sql, &self.defines)
        } else {
            stmt.sql
        };
        // A call (`exec p(1)` or `p(1)`) is profiled as a block.
        let block = match stmt.kind {
            StatementKind::PlSql => sql,
            _ => {
                let call = sql.trim().trim_end_matches(';');
                let call = call
                    .strip_prefix("exec ")
                    .or_else(|| call.strip_prefix("EXEC "))
                    .or_else(|| call.strip_prefix("execute "))
                    .or_else(|| call.strip_prefix("EXECUTE "))
                    .unwrap_or(call);
                let first = pythia_plsql::tokenize(call)
                    .into_iter()
                    .find(|t| !t.kind.is_trivia())
                    .map(|t| call[t.span].to_ascii_uppercase());
                if matches!(
                    first.as_deref(),
                    Some("SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "CREATE")
                ) {
                    self.warn("Profile works for PL/SQL blocks and procedure calls");
                    self.state.bottom = BottomTab::Messages;
                    return effects;
                }
                format!("begin\n  {call};\nend;")
            }
        };
        if !find_binds(&block).is_empty() {
            self.warn("Profile doesn't support bind variables yet; use literals");
            self.state.bottom = BottomTab::Messages;
            return effects;
        }
        let id = request_id();
        self.state.profile = Some((id, None));
        self.state.bottom = BottomTab::Profile;
        let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
            let profile = crate::profiler::run(conn, &block)
                .map_err(|e| pythia_db::DbError::new(crate::profiler::explain(&e)))?;
            Ok(Box::new(profile) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    /// Runs `EXPLAIN PLAN` for the current statement (on the main session).
    pub fn explain(&mut self) -> Effects {
        let mut effects = Effects::default();
        let Some(stmt) = self.statements(true).into_iter().next() else {
            return effects;
        };
        if stmt.kind != StatementKind::Sql {
            self.info("Explain plan works for SQL statements only");
            return effects;
        }
        let id = request_id();
        self.state.plan = Some(Plan {
            id,
            result: None,
            view: Default::default(),
        });
        self.state.bottom = BottomTab::Plan;
        let sql = substitute(&stmt.sql, &self.defines);
        effects.request(
            Target::Main,
            DbRequest::Meta {
                id,
                query: MetaQuery::ExplainPlan { sql },
            },
        );
        effects
    }

    /// Sends the next queued statement, handling SQL*Plus commands locally.
    fn run_next(&mut self, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        while let Some(mut stmt) = self.state.queue.pop_front() {
            if let Some(internal) = stmt.internal.take() {
                self.send_internal(stmt.start, stmt.sql, internal, cx, &mut effects);
                return effects;
            }
            if stmt.kind == StatementKind::SqlPlus
                && cx.dialect != pythia_db::Dialect::ORACLE
                && !client_command(&stmt.sql)
            {
                // MySQL's SET, SHOW, DESC, START TRANSACTION, … are SQL.
                stmt.kind = StatementKind::Sql;
            }
            if stmt.kind == StatementKind::SqlPlus {
                match self.sqlplus(&stmt) {
                    SqlPlus::Done => continue,
                    SqlPlus::Run(sql) => {
                        stmt.sql = sql;
                        stmt.kind = StatementKind::PlSql;
                    }
                    SqlPlus::Query(sql, binds) => {
                        self.send(stmt.start, sql, binds, cx, &mut effects);
                        return effects;
                    }
                }
            }
            if self.needs_values(&stmt.sql) {
                self.wait_for_values(stmt, false);
                return effects;
            }
            let (sql, binds) = self.with_values(&stmt.sql);
            self.send(stmt.start, sql, binds, cx, &mut effects);
            return effects;
        }
        effects
    }

    /// Variables a statement uses: substitutions first, then binds.
    fn statement_variables(&self, sql: &str) -> Vec<Variable> {
        let substitutions = if self.substitution {
            dedup(find_substitutions(sql).into_iter().map(|s| s.name))
        } else {
            Vec::new()
        };
        let binds = dedup(find_binds(sql).into_iter());
        substitutions
            .into_iter()
            .map(Variable::Substitution)
            .chain(binds.into_iter().map(Variable::Bind))
            .collect()
    }

    /// Whether a variable has been given a value (NULL counts).
    fn has_value(&self, variable: &Variable) -> bool {
        match variable {
            Variable::Bind(name) => self.bind_values.contains_key(&name.to_lowercase()),
            Variable::Substitution(name) => self.defines.contains_key(name),
        }
    }

    /// Whether a statement uses a variable that has no value yet.
    fn needs_values(&self, sql: &str) -> bool {
        self.statement_variables(sql)
            .iter()
            .any(|v| !self.has_value(v))
    }

    /// The statement with its substitutions replaced, and its binds with
    /// their values (empty if none was entered).
    fn with_values(&self, sql: &str) -> (String, Vec<(String, Option<String>)>) {
        let text = if self.substitution {
            substitute(sql, &self.defines)
        } else {
            sql.to_owned()
        };
        let binds = dedup(find_binds(sql).into_iter())
            .into_iter()
            .map(|name| {
                let value = self
                    .bind_values
                    .get(&name.to_lowercase())
                    .cloned()
                    .unwrap_or(Some(String::new()));
                (name, value)
            })
            .collect();
        (text, binds)
    }

    /// Holds the run until the Variables panel's Execute (or F8).
    fn wait_for_values(&mut self, statement: PendingStatement, debug: bool) {
        self.state.prompt_for_debug = debug;
        self.state.prompt = Some(VariablePrompt {
            statement,
            focus: true,
        });
    }

    /// Whether a statement waits for values in the Variables panel.
    pub fn waiting_for_values(&self) -> bool {
        self.state.prompt.is_some()
    }

    /// Drops the statement waiting for values, and the rest of the run.
    pub fn cancel_waiting(&mut self) {
        if self.state.prompt.take().is_some() {
            self.state.prompt_for_debug = false;
            self.state.queue.clear();
            self.info("Execution cancelled");
        }
    }

    /// Variables of the whole text, in order of appearance; worked out
    /// again when the text changes.
    pub fn variables(&mut self) -> &[Variable] {
        let key = (self.editor.revision(), self.substitution, self.state.syntax);
        if self.state.variables.as_ref().is_none_or(|(k, _)| *k != key) {
            let mut found: Vec<Variable> = Vec::new();
            let text = self.editor.text();
            for stmt in split_statements_in(text, self.state.syntax) {
                for v in self.statement_variables(stmt.text(text)) {
                    if !found.iter().any(|f| same_variable(f, &v)) {
                        found.push(v);
                    }
                }
            }
            self.state.variables = Some((key, found));
        }
        &self.state.variables.as_ref().expect("variables").1
    }

    fn send(
        &mut self,
        start: usize,
        sql: String,
        binds: Vec<(String, Option<String>)>,
        cx: &DocContext,
        effects: &mut Effects,
    ) {
        // Every statement of a run comes here, not only the first (#115).
        if cx.read_only && !is_query(&sql) {
            self.refuse_read_only();
            return;
        }
        let id = request_id();
        self.state.running = Some(Running {
            internal: None,
            start,
            sql: sql.clone(),
        });
        effects.request(
            Target::Own,
            DbRequest::Execute {
                id,
                sql,
                max_rows: cx.fetch_size,
                binds,
            },
        );
    }

    fn send_internal(
        &mut self,
        start: usize,
        sql: String,
        internal: Internal,
        cx: &DocContext,
        effects: &mut Effects,
    ) {
        let id = request_id();
        let request = match &internal {
            Internal::EditKey { table, .. } => {
                let table = table.clone();
                self.state.key_lookup = Some(id);
                DbRequest::Run {
                    id,
                    task: Box::new(move |client| {
                        let key = crate::data_edit::row_key(client, &table)?;
                        Ok(Box::new(key) as Box<dyn std::any::Any + Send>)
                    }),
                }
            }
            _ => DbRequest::Execute {
                id,
                sql: sql.clone(),
                max_rows: cx.fetch_size,
                binds: Vec::new(),
            },
        };
        self.state.running = Some(Running {
            start,
            sql,
            internal: Some(internal),
        });
        effects.request(Target::Own, request);
    }

    /// Re-runs result `index` with what identifies its rows (ROWIDs on
    /// Oracle, the table's key on MySQL) so it can be edited in place.
    pub fn start_editing(&mut self, index: usize, cx: &DocContext) -> Effects {
        if cx.read_only {
            self.info(READ_ONLY_EDIT);
            return Effects::default();
        }
        let Some(result) = self.state.results.get(index) else {
            return Effects::default();
        };
        let Some(table) = crate::data_edit::single_table(&result.sql) else {
            self.warn("Only results of a single-table query can be edited");
            return Effects::default();
        };
        if !crate::data_edit::can_edit(&result.sql) {
            self.warn("Results of DISTINCT, GROUP BY or aggregate queries can't be edited");
            return Effects::default();
        }
        let (source, replaces) = (result.sql.clone(), result.id);
        if cx.dialect.kind() == pythia_db::DbKind::Oracle {
            let edit = crate::data_edit::Editable {
                table,
                source,
                key: crate::data_edit::RowKey::Rowid,
            };
            self.queue_edit_query(edit, Some(replaces));
        } else {
            self.state.queue.push_back(PendingStatement {
                origin: None,
                kind: StatementKind::Sql,
                sql: String::new(),
                start: 0,
                internal: Some(Internal::EditKey {
                    table,
                    source,
                    replaces,
                }),
            });
        }
        self.run_next(cx)
    }

    fn queue_edit_query(&mut self, edit: crate::data_edit::Editable, replaces: Option<u64>) {
        let Some(sql) = crate::data_edit::edit_query(&edit) else {
            self.warn("Only results of a single-table query can be edited");
            return;
        };
        self.state.queue.push_back(PendingStatement {
            origin: None,
            kind: StatementKind::Sql,
            sql,
            start: 0,
            internal: Some(Internal::EditQuery { edit, replaces }),
        });
    }

    /// Generated DML for the pending changes of result `index`.
    fn pending_dml(&self, index: usize, cx: &DocContext) -> Vec<crate::data_edit::Dml> {
        let Some(result) = self.state.results.get(index) else {
            return Vec::new();
        };
        let (Some(edit), Some(changes)) = (&result.edit, result.grid.changes()) else {
            return Vec::new();
        };
        crate::data_edit::dml(cx.dialect, edit, &result.columns, &result.rows, changes)
    }

    /// Applies the pending changes of result `index` in this window's
    /// transaction, then refreshes the result.
    pub fn apply_changes(&mut self, index: usize, cx: &DocContext) -> Effects {
        if cx.read_only {
            self.info(READ_ONLY_EDIT);
            return Effects::default();
        }
        let statements = self.pending_dml(index, cx);
        let Some(result) = self.state.results.get(index) else {
            return Effects::default();
        };
        let Some(edit) = result.edit.clone() else {
            return Effects::default();
        };
        let replaces = Some(result.id);
        self.state.messages.clear();
        for dml in statements {
            self.state.queue.push_back(PendingStatement {
                origin: None,
                kind: StatementKind::Sql,
                sql: dml.sql,
                start: 0,
                internal: Some(Internal::Dml {
                    expect_rows: dml.expect_rows,
                    what: dml.what,
                }),
            });
        }
        self.queue_edit_query(edit, replaces);
        self.run_next(cx)
    }

    fn edit_action(&mut self, action: EditAction, cx: &DocContext) -> Effects {
        match action {
            EditAction::Start(i) => self.start_editing(i, cx),
            EditAction::Apply(i) => self.apply_changes(i, cx),
            EditAction::Discard(i) => {
                if let Some(r) = self.state.results.get_mut(i) {
                    r.grid.clear_changes();
                }
                Effects::default()
            }
            EditAction::Done(i) => {
                if let Some(r) = self.state.results.get_mut(i) {
                    r.grid.set_read_only();
                    r.edit = None;
                }
                Effects::default()
            }
            EditAction::ShowSql(i) => {
                let mut effects = Effects::default();
                let script: Vec<String> = self
                    .pending_dml(i, cx)
                    .into_iter()
                    .map(|d| format!("{};", d.sql))
                    .collect();
                if let Some(connection) = cx.connection.clone() {
                    effects.open.push(crate::documents::OpenRequest::Sql {
                        connection,
                        text: format!(
                            "-- Changes of {}\n{}\n",
                            self.state.results[i].title,
                            script.join("\n")
                        ),
                        run: false,
                    });
                }
                effects
            }
        }
    }

    /// Handles a SQL*Plus command; returns SQL to run instead, if any.
    fn sqlplus(&mut self, stmt: &PendingStatement) -> SqlPlus {
        let line = stmt.sql.trim();
        if let Some(file) = line
            .strip_prefix("@@")
            .map(|f| (f, true))
            .or_else(|| line.strip_prefix('@').map(|f| (f, false)))
        {
            return self.include(file.0, file.1, stmt);
        }
        let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match command.to_ascii_uppercase().as_str() {
            "EXEC" | "EXECUTE" => {
                SqlPlus::Run(format!("begin\n  {};\nend;", rest.trim_end_matches(';')))
            }
            "PRO" | "PROMPT" => {
                self.state.output.push(rest.to_owned());
                SqlPlus::Done
            }
            "DEF" | "DEFINE" => {
                match rest.split_once('=') {
                    Some((name, value)) => {
                        let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
                        self.defines
                            .insert(name.trim().to_lowercase(), value.to_owned());
                    }
                    None => {
                        let list: Vec<String> = self
                            .defines
                            .iter()
                            .map(|(k, v)| format!("DEFINE {k} = \"{v}\""))
                            .collect();
                        self.state.output.extend(list);
                    }
                }
                SqlPlus::Done
            }
            "UNDEF" | "UNDEFINE" => {
                for name in rest.split_whitespace() {
                    self.defines.remove(&name.to_lowercase());
                }
                SqlPlus::Done
            }
            "DESC" | "DESCRIBE" => {
                let name = rest.trim_end_matches(';').to_owned();
                let (owner, object) = match name.split_once('.') {
                    Some((o, n)) => (Some(o.to_owned()), n.to_owned()),
                    None => (None, name),
                };
                SqlPlus::Query(
                    DESCRIBE_SQL.into(),
                    vec![("name".into(), Some(object)), ("owner".into(), owner)],
                )
            }
            "SET" => {
                let lower = rest.to_ascii_lowercase();
                if let Some(v) = lower
                    .strip_prefix("define ")
                    .or_else(|| lower.strip_prefix("def "))
                {
                    self.substitution = v.trim() != "off";
                }
                SqlPlus::Done
            }
            "STA" | "START" => self.include(rest, false, stmt),
            "WHENEVER" => {
                let words: Vec<String> = rest
                    .split_whitespace()
                    .map(str::to_ascii_uppercase)
                    .collect();
                if words.first().is_some_and(|w| w == "SQLERROR") {
                    self.state.continue_on_error = words.get(1).is_some_and(|w| w == "CONTINUE");
                }
                SqlPlus::Done
            }
            "REM" | "REMARK" | "SPOOL" | "SPO" | "COLUMN" | "COL" | "CLEAR" | "PAUSE" => {
                SqlPlus::Done
            }
            _ => {
                let first = line.lines().next().unwrap_or_default();
                self.warn(format!(
                    "SQL*Plus command not supported in SQL windows: {first}"
                ));
                SqlPlus::Done
            }
        }
    }

    /// Queues the statements of a script file (`@file`, `@@file`, `START`).
    fn include(&mut self, file: &str, relative: bool, stmt: &PendingStatement) -> SqlPlus {
        let name = file.trim().trim_end_matches(';').trim_matches('"');
        let mut path = std::path::PathBuf::from(name);
        if path.extension().is_none() {
            path.set_extension("sql");
        }
        if path.is_relative() {
            // `@@` is relative to the including script, `@` to the window's file.
            let base = if relative {
                stmt.origin.as_deref().or(self.path.as_deref())
            } else {
                self.path.as_deref()
            };
            if let Some(dir) = base.and_then(|p| p.parent()) {
                path = dir.join(path);
            }
        }
        match crate::text_file::read(&path).map(|f| f.text) {
            Ok(text) => {
                let statements = split_statements_in(&text, self.state.syntax);
                self.state.total += statements.len();
                self.info(format!(
                    "Running {} ({} statements)",
                    path.display(),
                    statements.len()
                ));
                for s in statements.into_iter().rev() {
                    self.state.queue.push_front(PendingStatement {
                        kind: s.kind,
                        sql: s.text(&text).to_owned(),
                        start: usize::MAX,
                        internal: None,
                        origin: Some(path.clone()),
                    });
                }
            }
            Err(e) => {
                let error = DbError::new(format!("Cannot read {}: {e}", path.display()));
                self.error(&error);
                if !self.state.continue_on_error {
                    self.state.queue.clear();
                }
            }
        }
        SqlPlus::Done
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.state.messages.push(Message {
            level: Level::Info,
            text: text.into(),
            jump: None,
            stack: Vec::new(),
        });
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.state.messages.push(Message {
            level: Level::Warning,
            text: text.into(),
            jump: None,
            stack: Vec::new(),
        });
    }

    pub fn error(&mut self, error: &DbError) {
        let jump = self
            .state
            .running
            .as_ref()
            .filter(|r| r.start != usize::MAX)
            .and_then(|r| {
                let offset = error.offset? as usize;
                let pos = r.start + offset.min(r.sql.len());
                Some(pos..pos + 1)
            });
        let text = error.text();
        if let Some(jump) = &jump {
            self.editor.select(jump.clone());
        }
        let stack = self.stack(error);
        self.state.messages.push(Message {
            level: Level::Error,
            text,
            jump,
            stack,
        });
        self.state.bottom = BottomTab::Messages;
    }

    /// The frames of an error's PL/SQL stack that lead to code: stored units
    /// (looked up by a task [`Self::on_event`] sends) and the line of the
    /// statement that ran. Other anonymous blocks ran by dynamic SQL.
    fn stack(&mut self, error: &DbError) -> Vec<Frame> {
        let frames = error.stack();
        let id = request_id();
        let mut units = Vec::new();
        let mut stack = Vec::new();
        for (i, frame) in frames.iter().enumerate() {
            let target = match &frame.unit {
                Some((owner, name)) => {
                    units.push((owner.clone(), name.clone(), frame.line));
                    FrameTarget::Pending(id)
                }
                None if i + 1 == frames.len() => match self.statement_line(frame.line as usize) {
                    Some((range, text)) => FrameTarget::Statement { range, text },
                    None => continue,
                },
                None => continue,
            };
            stack.push(Frame {
                text_line: frame.text_line,
                target,
            });
        }
        if !units.is_empty() {
            self.state.stack_lookups.push((id, units));
        }
        stack
    }

    /// Line `line` of the running statement in the editor: the range of its
    /// text and the text.
    fn statement_line(&self, line: usize) -> Option<(Range<usize>, String)> {
        let running = self
            .state
            .running
            .as_ref()
            .filter(|r| r.start != usize::MAX)?;
        let text = self.editor.text();
        let first = text.get(..running.start)?.matches('\n').count() + 1;
        let mut start = crate::editor::line_start(text, first + line.max(1) - 1);
        if line <= 1 {
            start = start.max(running.start);
        }
        let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
        let code = text[start..end].trim_end();
        let indent = code.len() - code.trim_start().len();
        Some((start + indent..start + code.len(), code.trim().to_owned()))
    }

    pub fn on_event(&mut self, event: DbEvent, cx: &DocContext) -> Effects {
        let mut effects = self.handle_event(event, cx);
        for (id, units) in std::mem::take(&mut self.state.stack_lookups) {
            let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
                let found: Vec<_> = units
                    .into_iter()
                    .map(|(owner, name, line)| {
                        let source = pythia_db::error_stack::locate(conn, &owner, &name, line)
                            .unwrap_or_else(|e| {
                                pythia_db::error_stack::FrameSource::Hidden(e.text())
                            });
                        (owner, name, source)
                    })
                    .collect();
                Ok(Box::new(found) as Box<dyn std::any::Any + Send>)
            });
            effects.request(Target::Main, DbRequest::Task { id, task });
        }
        effects
    }

    /// The code of an error stack's stored units, found: the frames waiting
    /// for task `id` lead there.
    fn stack_found(&mut self, id: u64, result: pythia_db::TaskResult) {
        use pythia_db::error_stack::FrameSource;
        let mut found = match result.map(|any| any.downcast::<Vec<(String, String, FrameSource)>>())
        {
            Ok(Ok(found)) => Ok(found.into_iter()),
            Ok(Err(_)) => Err("unexpected result".to_owned()),
            Err(e) => Err(e.text()),
        };
        let frames = self
            .state
            .messages
            .iter_mut()
            .flat_map(|m| &mut m.stack)
            .filter(|f| f.target == FrameTarget::Pending(id));
        for frame in frames {
            frame.target = match found.as_mut().map(Iterator::next) {
                Ok(Some((
                    owner,
                    name,
                    FrameSource::Source {
                        object_type,
                        line,
                        text,
                    },
                ))) => FrameTarget::Program {
                    object: ObjectRef {
                        owner,
                        name,
                        object_type,
                    },
                    line: line as usize,
                    text,
                },
                Ok(Some((_, _, FrameSource::Hidden(why)))) => FrameTarget::Hidden(why),
                Ok(None) => FrameTarget::Hidden("unexpected result".into()),
                Err(why) => FrameTarget::Hidden(why.clone()),
            };
        }
    }

    fn handle_event(&mut self, event: DbEvent, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        match event {
            DbEvent::QueryStarted { id, columns } => {
                let running = self.state.running.as_ref();
                let sql = running.map(|r| r.sql.clone()).unwrap_or_default();
                let mut result = QueryResult {
                    id,
                    title: String::new(),
                    sql,
                    columns,
                    rows: Vec::new(),
                    more: true,
                    elapsed: Duration::ZERO,
                    grid: GridState::default(),
                    edit: None,
                    filter: String::new(),
                    chart: None,
                    relations: Vec::new(),
                    link_relations: Vec::new(),
                };
                result.grid.dialect = cx.dialect;
                let mut replace = None;
                if let Some(Internal::EditQuery { edit, replaces }) =
                    running.and_then(|r| r.internal.as_ref())
                {
                    let key_start = result.columns.len().saturating_sub(edit.key.width());
                    result.grid.set_editable(Some(key_start));
                    result.edit = Some(edit.clone());
                    result.sql = edit.source.clone();
                    replace = replaces
                        .and_then(|old| self.state.results.iter().position(|r| r.id == old));
                }
                let index = match replace {
                    Some(i) => {
                        result.title = std::mem::take(&mut self.state.results[i].title);
                        self.state.results[i] = result;
                        i
                    }
                    None => {
                        result.title = format!("Result {}", self.state.results.len() + 1);
                        self.state.results.push(result);
                        self.state.results.len() - 1
                    }
                };
                self.state.bottom = BottomTab::Result(index);
                if cx.dialect.supports(Feature::FollowKeys)
                    && let Some(table) =
                        crate::data_edit::single_table(&self.state.results[index].sql)
                {
                    let task_id = request_id();
                    self.state.relation_requests.insert(task_id, id);
                    let task: pythia_db::TaskFn =
                        Box::new(move |conn: &pythia_db::oracle::Connection| {
                            let relations = crate::relations::load(conn, &table)?;
                            Ok(Box::new(relations) as Box<dyn std::any::Any + Send>)
                        });
                    effects.request(Target::Main, DbRequest::Task { id: task_id, task });
                }
            }
            DbEvent::TaskDone { id, result }
                if self.state.profile.as_ref().is_some_and(|(p, _)| *p == id) =>
            {
                let profile = match result.map(|any| any.downcast::<crate::profiler::Profile>()) {
                    Ok(Ok(profile)) => {
                        self.warn(
                            "Profiling committed the transaction (DBMS_HPROF writes its trace data)",
                        );
                        Ok(*profile)
                    }
                    Ok(Err(_)) => Err("unexpected result".to_owned()),
                    Err(e) => Err(e.to_string()),
                };
                self.state.profile = Some((id, Some(profile)));
            }
            DbEvent::TaskDone { id, result }
                if self.state.to_file.as_ref().is_some_and(|t| t.id == id) =>
            {
                let Some(job) = self.state.to_file.take() else {
                    return effects;
                };
                match result.map(|any| any.downcast::<u64>().map(|n| *n).unwrap_or(0)) {
                    Ok(rows) => {
                        effects.executed.push(job.sql);
                        self.info(format!(
                            "{rows} rows written to {} in {}",
                            job.path.display(),
                            fmt_duration(job.started.elapsed())
                        ));
                    }
                    Err(e) => {
                        self.state.messages.push(Message {
                            level: Level::Error,
                            text: format!("Execute to file failed: {e}"),
                            jump: None,
                            stack: Vec::new(),
                        });
                    }
                }
                self.state.bottom = BottomTab::Messages;
            }
            DbEvent::TaskDone { id, result } if self.state.key_lookup == Some(id) => {
                self.state.key_lookup = None;
                let running = self.state.running.take();
                let Some(Internal::EditKey {
                    table,
                    source,
                    replaces,
                }) = running.and_then(|r| r.internal)
                else {
                    return effects;
                };
                let key = result.and_then(|any| {
                    any.downcast::<Vec<String>>()
                        .map(|key| *key)
                        .map_err(|_| DbError::new("unexpected result"))
                });
                match key {
                    Ok(key) if key.is_empty() => {
                        self.state.queue.clear();
                        self.warn(format!(
                            "The rows of {table} can't be edited: it has no primary key (or \
                             unique key on NOT NULL columns) to find them by"
                        ));
                        self.state.bottom = BottomTab::Messages;
                    }
                    Ok(key) => {
                        let edit = crate::data_edit::Editable {
                            table,
                            source,
                            key: crate::data_edit::RowKey::Columns(key),
                        };
                        self.queue_edit_query(edit, Some(replaces));
                        effects.extend(self.run_next(cx));
                    }
                    Err(e) => {
                        self.state.queue.clear();
                        self.error(&e);
                    }
                }
            }
            DbEvent::TaskDone { id, result } if self.state.relation_requests.contains_key(&id) => {
                let result_id = self.state.relation_requests.remove(&id).unwrap_or_default();
                let relations = result
                    .ok()
                    .and_then(|any| any.downcast::<Vec<crate::relations::Relation>>().ok())
                    .map(|r| *r)
                    .unwrap_or_default();
                if let Some(result) = self.state.results.iter_mut().find(|r| r.id == result_id) {
                    let (links, index) = crate::relations::links(&relations, &result.columns);
                    result.grid.links = links;
                    result.link_relations = index;
                    result.relations = relations;
                }
            }
            DbEvent::Rows {
                id,
                rows,
                more,
                elapsed,
            } => {
                let Some(result) = self.state.results.iter_mut().find(|r| r.id == id) else {
                    return effects;
                };
                result.rows.extend(rows);
                result.more = more;
                result.elapsed += elapsed;
                let fetched = result.rows.len();
                if self.state.fetching == Some(id) {
                    self.state.fetching = None;
                    return effects;
                }
                self.state.done += 1;
                if let Some(running) = self.state.running.take()
                    && running.internal.is_none()
                {
                    effects.executed.push(running.sql);
                }
                let suffix = if more { " (more available)" } else { "" };
                let rows = if fetched == 1 {
                    "1 row".to_owned()
                } else {
                    format!("{fetched} rows")
                };
                self.info(format!(
                    "{rows} selected in {}{suffix}",
                    fmt_duration(elapsed)
                ));
                effects.extend(self.run_next(cx));
            }
            DbEvent::Executed {
                summary, elapsed, ..
            } => {
                let running = self.state.running.take();
                self.state.done += 1;
                if let Some(Internal::Dml { expect_rows, what }) =
                    running.as_ref().and_then(|r| r.internal.clone())
                {
                    if summary.rows_affected != Some(expect_rows) {
                        let skipped = std::mem::take(&mut self.state.queue).len();
                        self.state.messages.push(Message {
                            level: Level::Error,
                            text: format!(
                                "Conflict on {what}: it was changed or deleted by another session since it was fetched. \
                                 Nothing after it was applied ({skipped} statement(s) skipped); roll back or refresh and retry."
                            ),
                            jump: None,
                            stack: Vec::new(),
                        });
                        self.state.bottom = BottomTab::Messages;
                        return effects;
                    }
                    if !self
                        .state
                        .queue
                        .iter()
                        .any(|s| matches!(s.internal, Some(Internal::Dml { .. })))
                    {
                        self.info("Changes applied — commit (F10) or roll back (Shift+F10) to finish the transaction");
                    }
                    effects.extend(self.run_next(cx));
                    return effects;
                }
                let failed = summary
                    .compiled
                    .as_ref()
                    .is_some_and(|c| c.errors.iter().any(|e| !e.warning));
                let text = format!("{summary} in {}", fmt_duration(elapsed));
                if failed {
                    self.warn(text);
                } else {
                    self.info(text);
                }
                if let (Some(compiled), Some(running)) = (&summary.compiled, &running) {
                    self.compile_errors(compiled, running);
                }
                if let Some(running) = running {
                    effects.executed.push(running.sql);
                }
                effects.extend(self.run_next(cx));
            }
            DbEvent::Failed { id, error, elapsed } => {
                if self.state.fetching == Some(id) {
                    self.state.fetching = None;
                    self.error(&error);
                    return effects;
                }
                self.error(&error);
                self.state.running = None;
                self.state.done += 1;
                if self.state.continue_on_error && !self.state.queue.is_empty() {
                    self.info(format!(
                        "Failed after {} — continuing (WHENEVER SQLERROR CONTINUE)",
                        fmt_duration(elapsed)
                    ));
                    effects.extend(self.run_next(cx));
                    return effects;
                }
                let dropped = std::mem::take(&mut self.state.queue).len();
                self.info(format!("Failed after {}", fmt_duration(elapsed)));
                if dropped > 0 {
                    self.info(format!("{dropped} remaining statement(s) skipped"));
                }
            }
            DbEvent::TaskDone { id, result }
                if self
                    .state
                    .statistics
                    .as_ref()
                    .is_some_and(|(s, _)| *s == id) =>
            {
                let report = match result.map(|any| any.downcast::<crate::statistics::Report>()) {
                    Ok(Ok(report)) => Ok(*report),
                    Ok(Err(_)) => Err("unexpected result".to_owned()),
                    Err(e) => Err(e.to_string()),
                };
                self.state.statistics = Some((id, Some(report)));
            }
            DbEvent::Meta { id, result } => {
                if let Some(plan) = self.state.plan.as_mut().filter(|p| p.id == id) {
                    plan.result = Some(match result {
                        Ok(MetaResult::Table { columns, rows }) => {
                            Ok(pythia_ui::plan::rows(&columns, &rows))
                        }
                        Ok(_) => Err("unexpected result".into()),
                        Err(e) => Err(e.to_string()),
                    });
                }
            }
            DbEvent::Output(lines) => {
                self.state.output.extend(lines);
                if self.state.results.is_empty() {
                    self.state.bottom = BottomTab::Output;
                }
            }
            DbEvent::Notice(text) => self.info(text),
            DbEvent::Error(error) | DbEvent::ConnectFailed(error) => {
                self.state.running = None;
                self.state.fetching = None;
                self.state.queue.clear();
                self.error(&error);
            }
            DbEvent::TaskDone { id, result }
                if self
                    .state
                    .messages
                    .iter()
                    .flat_map(|m| &m.stack)
                    .any(|f| f.target == FrameTarget::Pending(id)) =>
            {
                self.stack_found(id, result);
            }
            _ => {}
        }
        effects
    }

    fn compile_errors(&mut self, compiled: &pythia_db::Compiled, running: &Running) {
        let text = self.editor.text();
        // ALL_ERRORS lines are relative to the unit's source, whose line 1 is
        // the `CREATE ...` line of the statement.
        let base_line = text[..running.start.min(text.len())].matches('\n').count();
        let mut markers = Vec::new();
        let mut messages = Vec::new();
        for e in &compiled.errors {
            let line = base_line + e.line as usize;
            let start = crate::editor::line_start(text, line);
            let col = text[start..]
                .char_indices()
                .nth(e.position.saturating_sub(1) as usize)
                .map_or(start, |(i, _)| start + i);
            let level = if e.warning {
                Level::Warning
            } else {
                Level::Error
            };
            messages.push(Message {
                level,
                text: format!("  {}:{}  {}", e.line, e.position, e.text),
                jump: Some(col..col + 1),
                stack: Vec::new(),
            });
            markers.push(Marker {
                line,
                kind: if e.warning {
                    MarkerKind::Warning
                } else {
                    MarkerKind::Error
                },
                message: e.text.clone(),
            });
        }
        self.state.messages.extend(messages);
        self.editor.set_markers(markers);
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        self.state.syntax = cx.dialect.syntax();
        egui::Panel::bottom(egui::Id::new(("sql-bottom", cx.id)))
            .resizable(true)
            .default_size(ui.available_height() * 0.45)
            .min_size(80.0)
            .show(ui, |ui| effects.extend(self.bottom_ui(ui, cx)));
        if self.waiting_for_values() || (self.show_variables && !self.variables().is_empty()) {
            egui::Panel::right(egui::Id::new(("sql-variables", cx.id)))
                .resizable(true)
                .default_size(340.0)
                .min_size(200.0)
                .show(ui, |ui| effects.extend(self.variables_ui(ui, cx)));
        }
        // The editor fills the tab to its edges.
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let mut out = self
                .editor
                .ui(ui, ("sql-editor", cx.id), &cx.settings.editor);
            // While debugging a block, hovering a variable shows its value.
            if let (Some(offset), Some(debug)) = (out.hover_request, cx.debug) {
                let block = pythia_debugger::Unit::new("", "", pythia_debugger::UnitType::Block);
                if crate::editor::debug_hover(&mut self.editor, offset, debug, &block) {
                    out.hover_request = None;
                }
            }
            if let (Some(start), Some(debug)) = (self.state.debug_start, cx.debug) {
                let line = block_line(debug).map(|l| (start + l as usize - 1) as u32);
                self.editor.set_debug_lines(&[], line);
            }
            effects.extend(self.assist.handle(&mut self.editor, &out, cx));
            effects.extend(crate::editor_menu::show(
                &out.response,
                &mut self.editor,
                &mut self.assist,
                None,
                cx,
            ));
            self.editor
                .format_as_typed(&out, cx.settings.format_auto, &cx.settings.format);
            if let Some(from) = self.assist.take_jumped_from() {
                effects.jumped_from = Some((0, from));
            }
            if let Some(places) = self.assist.take_references() {
                let text = self.editor.text();
                effects.references = Some(
                    places
                        .into_iter()
                        .filter_map(|place| match place {
                            crate::assist::Place::Here(r) => {
                                Some(crate::documents::Reference::new(0, r, text, None))
                            }
                            crate::assist::Place::Companion(_) => None,
                        })
                        .collect(),
                );
            }
            // Syntax errors come from the PL/SQL parser.
            let check = cx.dialect.supports(Feature::PlSql);
            self.assist.background(&mut self.editor, ui, check);
            self.state.line_col = self.editor.line_col();
            // The toolbar was drawn before this frame's edits.
            if self.refresh_debuggable() {
                ui.ctx().request_repaint();
            }
        });
        effects
    }

    fn bottom_ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = self.poll_to_file(ui);
        let mut edit_action = None;
        let variables = self.variables().len();
        let show_variables = &mut self.show_variables;
        let state = &mut self.state;
        ui.horizontal(|ui| {
            for (i, result) in state.results.iter().enumerate() {
                ui.selectable_value(&mut state.bottom, BottomTab::Result(i), &result.title);
            }
            if state.plan.is_some() {
                ui.selectable_value(&mut state.bottom, BottomTab::Plan, "Plan");
            }
            if state.statistics.is_some() {
                ui.selectable_value(&mut state.bottom, BottomTab::Statistics, "Statistics");
            }
            if state.profile.is_some() {
                ui.selectable_value(&mut state.bottom, BottomTab::Profile, "Profile");
            }
            let label = match state.messages.iter().map(|m| m.level).max() {
                Some(Level::Error) => RichText::new("Messages").color(ui.visuals().error_fg_color),
                Some(Level::Warning) => RichText::new("Messages").color(ui.visuals().warn_fg_color),
                _ => "Messages".into(),
            };
            ui.selectable_value(&mut state.bottom, BottomTab::Messages, label);
            // DBMS_OUTPUT; other databases print nothing there.
            if cx.dialect.supports(Feature::ServerOutput) || !state.output.is_empty() {
                let output = if state.output.is_empty() {
                    "Output".to_owned()
                } else {
                    format!("Output ({})", state.output.len())
                };
                ui.selectable_value(&mut state.bottom, BottomTab::Output, output);
            }
            if let Some(job) = &state.to_file {
                ui.spinner();
                let rows = job.rows.load(std::sync::atomic::Ordering::Relaxed);
                let name = job
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ui.label(format!("Writing {rows} rows to {name}…"))
                    .on_hover_text(job.path.display().to_string());
                if icon_text_button(ui, Icon::Stop, "Break")
                    .on_hover_text("Shift+Esc; the incomplete file is removed")
                    .clicked()
                {
                    effects.cancel = true;
                }
            } else if state.running.is_some() || state.fetching.is_some() {
                ui.spinner();
                if state.total > 1 {
                    ui.add(
                        egui::ProgressBar::new(state.done as f32 / state.total as f32)
                            .desired_width(140.0)
                            .text(format!("{} / {}", state.done + 1, state.total)),
                    );
                } else {
                    ui.label("Executing…");
                }
                if icon_text_button(ui, Icon::Stop, "Break")
                    .on_hover_text("Shift+Esc")
                    .clicked()
                {
                    effects.cancel = true;
                }
            }
            if variables > 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.toggle_value(show_variables, format!("Variables ({variables})"))
                        .on_hover_text("Show or hide the Variables panel");
                });
            }
        });
        if let Some(rx) = &state.export
            && let Ok(result) = rx.try_recv()
        {
            state.export = None;
            let message = match result {
                Ok(Some(path)) => Message {
                    level: Level::Info,
                    text: format!("Exported to {}", path.display()),
                    jump: None,
                    stack: Vec::new(),
                },
                Ok(None) => Message {
                    level: Level::Info,
                    text: "Export cancelled".into(),
                    jump: None,
                    stack: Vec::new(),
                },
                Err(e) => Message {
                    level: Level::Error,
                    text: format!("Export failed: {e}"),
                    jump: None,
                    stack: Vec::new(),
                },
            };
            state.messages.push(message);
        }
        if let BottomTab::Result(i) = state.bottom
            && let Some(result) = state.results.get_mut(i)
        {
            // Buttons first, from the right; the filter and the row count get
            // what's left. Without room for them the buttons drop their
            // labels, judged by the last frame's widths.
            let bar_id = ui.id().with(("result-bar", i));
            let total = ui.available_width();
            let saved: ResultBar = ui.data(|d| d.get_temp(bar_id)).unwrap_or_default();
            let mut bar = saved;
            if bar.compact && total - bar.full_width >= ResultBar::MIN_LEFT {
                bar.compact = false;
            }
            let compact = bar.compact;
            let labelled = move |ui: &mut Ui, icon: Icon, text: &str| {
                if compact {
                    icon_button(ui, icon, text)
                } else {
                    icon_text_button(ui, icon, text)
                }
            };
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let idle = state.fetching.is_none() && state.running.is_none() && state.queue.is_empty();
                    match result.grid.changes() {
                        Some(changes) => {
                            let n = changes.count();
                            if labelled(ui, Icon::Close, "Done").on_hover_text("Stop editing").clicked() {
                                edit_action = Some(EditAction::Done(i));
                            }
                            if ui.add_enabled(n > 0 && idle, egui::Button::new("Discard")).clicked() {
                                edit_action = Some(EditAction::Discard(i));
                            }
                            if ui
                                .add_enabled(n > 0, egui::Button::new("Show SQL"))
                                .on_hover_text("Open the statements that would be executed in a new SQL window")
                                .clicked()
                            {
                                edit_action = Some(EditAction::ShowSql(i));
                            }
                            let label = match n {
                                0 => "Apply".to_owned(),
                                1 => "Apply 1 change".to_owned(),
                                n => format!("Apply {n} changes"),
                            };
                            let apply = egui::Button::image_and_text(Icon::Commit.image(ui), label);
                            if ui
                                .add_enabled(n > 0 && idle, apply)
                                .on_hover_text(
                                    "Run the changes in this window's transaction; rows changed by others are \
                                     detected and reported. Commit with F10 afterwards.",
                                )
                                .clicked()
                            {
                                edit_action = Some(EditAction::Apply(i));
                            }
                            ui.separator();
                        }
                        None => {
                            if idle
                                && !cx.read_only
                                && cx.dialect.supports(Feature::EditResults)
                                && crate::data_edit::can_edit(&result.sql)
                                && labelled(ui, Icon::Edit, "Edit")
                                    .on_hover_text(
                                        "Edit the rows of this result. Rows are not locked; conflicting \
                                         changes by other sessions are detected when applying.",
                                    )
                                    .clicked()
                            {
                                edit_action = Some(EditAction::Start(i));
                            }
                        }
                    }
                    let charting = result.chart.is_some();
                    let (icon, label, hint) = if charting {
                        (Icon::Grid, "Grid", "Back to the rows")
                    } else {
                        (Icon::Chart, "Chart", "Show the fetched rows as a bar or line chart")
                    };
                    if result.grid.changes().is_none()
                        && labelled(ui, icon, label).on_hover_text(hint).clicked()
                    {
                        result.chart = match result.chart {
                            Some(_) => None,
                            None => Some(crate::chart_view::ChartState::guess(
                                &result.columns,
                                result.rows.len(),
                            )),
                        };
                    }
                    let exporting = state.export.is_some();
                    ui.add_enabled_ui(!exporting, |ui| {
                        let formats = |ui: &mut Ui| {
                            for format in pythia_data::Format::ALL {
                                if ui.button(format.label()).clicked() {
                                    // The selection if there is one, else all fetched rows.
                                    let (columns, rows) =
                                        selection_data(&result.columns, &result.rows, &result.grid);
                                    state.export =
                                        Some(export_in_background(*format, columns, rows, cx.dialect));
                                    ui.close();
                                }
                            }
                        };
                        if compact {
                            let button = egui::Button::image(Icon::Export.image(ui));
                            egui::containers::menu::MenuButton::from_button(button)
                                .ui(ui, formats)
                                .0
                                .on_hover_text("Export");
                        } else {
                            ui.menu_button("Export", formats);
                        }
                    });
                    if labelled(ui, Icon::Copy, "Copy all")
                        .on_hover_text("Copy fetched rows as tab separated text")
                        .clicked()
                    {
                        let (columns, rows) =
                            shown_data(&result.columns, &result.rows, &result.grid);
                        ui.ctx().copy_text(to_tsv(&columns, &rows));
                    }
                    let idle = state.fetching.is_none() && state.running.is_none();
                    // Compact as a word: its icon is also the grid's.
                    let fetch_all = |ui: &mut Ui| {
                        if compact {
                            let all = egui::Button::new("All").frame_when_inactive(false).min_size(egui::vec2(0.0, 24.0));
                            ui.add(all).on_hover_text("Fetch all rows")
                        } else {
                            icon_text_button(ui, Icon::Grid, "Fetch all")
                        }
                    };
                    if result.more && idle && fetch_all(ui).clicked() {
                        state.fetching = Some(result.id);
                        effects.request(
                            Target::Own,
                            DbRequest::FetchMore {
                                id: result.id,
                                max_rows: usize::MAX,
                            },
                        );
                    }
                    if !compact {
                        bar.full_width = total - ui.available_width();
                        bar.compact = ui.available_width() < ResultBar::MIN_LEFT;
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        // The row count wants ~100 px; the filter never more than there is.
                        let room = ui.available_width();
                        let width = (room - 100.0).clamp(80.0, 160.0).min(room - 8.0).max(0.0);
                        ui.add(egui::TextEdit::singleline(&mut result.filter).hint_text("Filter rows…").desired_width(width));
                        result.grid.set_filter(&result.filter);
                        let more = if result.more { "+" } else { "" };
                        let rows = if result.filter.trim().is_empty() {
                            format!("{}{more} rows", result.rows.len())
                        } else {
                            format!("{} of {}{more} rows", result.grid.visible_rows(), result.rows.len())
                        };
                        let info = match &state.selection_summary {
                            Some(summary) => summary.clone(),
                            None => format!("{rows} · {} columns · {}", result.columns.len(), fmt_duration(result.elapsed)),
                        };
                        // Too little room even for "…": the status bar has the count.
                        if ui.available_width() >= 40.0 {
                            ui.add(egui::Label::new(RichText::new(info).weak()).truncate())
                                .on_hover_text(&result.sql);
                        }
                    });
                });
            });
            if bar != saved {
                ui.data_mut(|d| d.insert_temp(bar_id, bar));
                ui.ctx().request_repaint();
            }
        }
        ui.separator();

        match state.bottom {
            BottomTab::Result(i) if i < state.results.len() && state.results[i].chart.is_some() => {
                let result = &mut state.results[i];
                if let Some(chart) = result.chart.as_mut() {
                    crate::chart_view::ui(
                        ui,
                        chart,
                        &result.columns,
                        &result.rows,
                        &mut result.grid,
                        egui::Id::new(("chart", cx.id, result.id)),
                    );
                }
            }
            BottomTab::Result(i) if i < state.results.len() => {
                let result = &mut state.results[i];
                let out = ResultGrid::new(
                    &result.columns,
                    &result.rows,
                    &mut result.grid,
                    ("grid", cx.id, result.id),
                )
                .show(ui);
                state.selection_summary = out.selection_summary;
                if let Some((link, row)) = result.grid.take_follow()
                    && let Some(relation) = result
                        .link_relations
                        .get(link)
                        .and_then(|&r| result.relations.get(r))
                    && let Some(connection) = cx.connection.clone()
                {
                    effects.open.push(crate::documents::OpenRequest::Sql {
                        connection,
                        text: crate::relations::query(relation, &result.columns, &result.rows[row]),
                        run: true,
                    });
                }
                let result = &state.results[i];
                let is_open = state.results.len() == i + 1 && result.more;
                if out.reached_end && is_open && state.running.is_none() && state.fetching.is_none()
                {
                    state.fetching = Some(result.id);
                    effects.request(
                        Target::Own,
                        DbRequest::FetchMore {
                            id: result.id,
                            max_rows: cx.fetch_size,
                        },
                    );
                }
            }
            BottomTab::Plan => match state.plan.as_mut() {
                Some(Plan {
                    result: Some(Ok(rows)),
                    view,
                    ..
                }) => {
                    if !rows.is_empty()
                        && crate::ai::button(
                            ui,
                            &cx.settings.ai,
                            "Analyze with AI",
                            "Explain the plan of the current statement and point out problems",
                        )
                    {
                        effects
                            .commands
                            .push(crate::commands::Command::AiAnalyzePlan);
                    }
                    view.ui(ui, rows, egui::Id::new(("plan", cx.id)));
                }
                Some(Plan {
                    result: Some(Err(e)),
                    ..
                }) => {
                    ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                }
                _ => {
                    ui.spinner();
                }
            },
            BottomTab::Profile => match &state.profile {
                Some((_, Some(Ok(profile)))) => {
                    if let Some((object, line)) = crate::profiler::ui(ui, profile)
                        && let Some(connection) = cx.connection.clone()
                    {
                        effects.open.push(crate::documents::OpenRequest::ProgramAt {
                            connection,
                            object,
                            line,
                        });
                    }
                }
                Some((_, Some(Err(e)))) => {
                    ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Profiling…");
                    });
                }
            },
            BottomTab::Statistics => match &state.statistics {
                Some((_, Some(Ok(report)))) => {
                    if crate::ai::button(
                        ui,
                        &cx.settings.ai,
                        "Analyze with AI",
                        "Explain where the time goes, from the actual plan and statistics",
                    ) {
                        effects
                            .commands
                            .push(crate::commands::Command::AiAnalyzePlan);
                    }
                    statistics_ui(
                        ui,
                        report,
                        &mut state.statistics_view,
                        &mut state.statistics_text,
                    )
                }
                Some((_, Some(Err(e)))) => {
                    ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Running and fetching all rows…");
                    });
                }
            },
            BottomTab::Result(_) | BottomTab::Messages => {
                let mut jump = None;
                let mut open = None;
                // The innermost frame of the last error whose code can be
                // shown.
                let raised = state
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.level == Level::Error)
                    .and_then(|m| m.stack.iter().find(|f| f.target.opens()))
                    .map(|f| f.target.clone());
                let fix = state.messages.iter().any(|m| m.level == Level::Error)
                    && state.running.is_none()
                    && !cx.settings.ai.backends.is_empty();
                if fix || raised.is_some() {
                    ui.horizontal(|ui| {
                        if fix
                            && crate::ai::button(
                                ui,
                                &cx.settings.ai,
                                "Fix with AI",
                                "Find the cause of the error and suggest a corrected statement",
                            )
                        {
                            effects.commands.push(crate::commands::Command::AiFix);
                        }
                        if let Some(target) = raised
                            && icon_text_button(ui, Icon::Package, "Open where raised")
                                .on_hover_ui(|ui| frame_hint(ui, &target))
                                .clicked()
                        {
                            open = Some(target);
                        }
                    });
                }
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for m in &state.messages {
                            let color = match m.level {
                                Level::Error => ui.visuals().error_fg_color,
                                Level::Warning => ui.visuals().warn_fg_color,
                                Level::Info => ui.visuals().text_color(),
                            };
                            if !m.stack.is_empty() {
                                if let Some(target) = stack_message_ui(ui, m, color, &mut jump) {
                                    open = Some(target);
                                }
                                continue;
                            }
                            let text = RichText::new(&m.text).monospace().color(color);
                            match &m.jump {
                                Some(range) => {
                                    if ui.link(text).on_hover_text("Go to error").clicked() {
                                        jump = Some(range.clone());
                                    }
                                }
                                None => {
                                    ui.label(text);
                                }
                            }
                        }
                    });
                match open {
                    Some(FrameTarget::Program { object, line, .. }) => {
                        if let Some(connection) = cx.connection.clone() {
                            effects.open.push(OpenRequest::ProgramAt {
                                connection,
                                object,
                                line,
                            });
                        }
                    }
                    Some(FrameTarget::Statement { range, .. }) => jump = Some(range),
                    _ => {}
                }
                if let Some(range) = jump {
                    self.editor.select(range);
                }
            }
            BottomTab::Output => {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &state.output {
                            ui.label(RichText::new(line).monospace());
                        }
                    });
            }
        }
        if let Some(action) = edit_action {
            effects.extend(self.edit_action(action, cx));
        }
        effects
    }

    /// The Variables panel: the text's variables with their values, and
    /// the statement waiting for values, if any (#110).
    fn variables_ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let waiting = self
            .state
            .prompt
            .as_ref()
            .map(|p| self.statement_variables(&p.statement.sql))
            .unwrap_or_default();
        // A statement from an `@script` can have variables the text hasn't.
        let mut variables = waiting.clone();
        for v in self.variables().to_vec() {
            if !variables.iter().any(|w| same_variable(w, &v)) {
                variables.push(v);
            }
        }
        let mut decision = None;
        ui.horizontal(|ui| {
            ui.strong("Variables").on_hover_text(
                "Runs use these values, and the window keeps them. A statement with a \
                 variable that has no value waits here for it.",
            );
            if self.state.prompt.is_none() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(
                        ui,
                        Icon::Close,
                        "Hide; Variables below the editor shows it again",
                    )
                    .clicked()
                    {
                        self.show_variables = false;
                    }
                });
            }
        });
        if let Some(prompt) = &self.state.prompt {
            egui::Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .stroke(egui::Stroke::new(1.0, ui.visuals().warn_fg_color))
                .corner_radius(4)
                .inner_margin(6)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let place = match &prompt.statement.origin {
                        Some(path) => path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        None => format!("line {}", self.line_of(prompt.statement.start)),
                    };
                    ui.label(format!(
                        "Waiting for values to run the statement at {place}:"
                    ));
                    ui.add(
                        egui::Label::new(
                            RichText::new(first_line(&prompt.statement.sql))
                                .monospace()
                                .weak(),
                        )
                        .truncate(),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Execute").on_hover_text("Enter or F8").clicked() {
                            decision = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            decision = Some(false);
                        }
                    });
                });
            ui.add_space(4.0);
        }
        let mut focus = self
            .state
            .prompt
            .as_mut()
            .is_some_and(|p| std::mem::take(&mut p.focus));
        let mut enter = false;
        // One row per variable: name, value, NULL. Names take what the
        // longest needs, up to 45% of the panel.
        let gap = ui.spacing().item_spacing.x;
        let height = ui.spacing().interact_size.y;
        let null_width = text_width(ui, RichText::new("NULL").small());
        let name_width = variables
            .iter()
            .map(|v| text_width(ui, RichText::new(v.label()).monospace()))
            .fold(0.0, f32::max)
            .min(ui.available_width() * 0.45);
        let value_width = (ui.available_width() - name_width - null_width - 2.0 * gap).max(40.0);
        let cell = |ui: &mut Ui, width: f32, add: &mut dyn FnMut(&mut Ui)| {
            ui.allocate_ui_with_layout(
                egui::vec2(width, height),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(width);
                    add(ui)
                },
            );
        };
        ui.horizontal(|ui| {
            cell(ui, name_width, &mut |ui| {
                ui.label(RichText::new("Name").small().weak());
            });
            cell(ui, value_width, &mut |ui| {
                ui.label(RichText::new("Value").small().weak());
            });
            ui.label(RichText::new("NULL").small().weak());
        });
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                for v in &variables {
                    let missing = !self.has_value(v);
                    let asked = missing && waiting.iter().any(|w| same_variable(w, v));
                    let label = v.label();
                    let mut name = RichText::new(&label).monospace();
                    if asked {
                        name = name.color(ui.visuals().warn_fg_color);
                    }
                    let hint = if missing { "no value" } else { "" };
                    ui.horizontal(|ui| {
                        cell(ui, name_width, &mut |ui| {
                            ui.add(egui::Label::new(name.clone()).truncate())
                                .on_hover_text(&label);
                        });
                        let response = match v {
                            Variable::Bind(bind) => {
                                let key = bind.to_lowercase();
                                let current = self.bind_values.get(&key).cloned();
                                let mut is_null = matches!(current, Some(None));
                                let mut text = current.flatten().unwrap_or_default();
                                let field = egui::TextEdit::singleline(&mut text)
                                    .id_salt(("bind", &key))
                                    .hint_text(hint);
                                let r = ui.add_enabled_ui(!is_null, |ui| {
                                    ui.add_sized([value_width, height], field)
                                });
                                let r = r.inner;
                                if r.changed() {
                                    self.bind_values.insert(key.clone(), Some(text.clone()));
                                }
                                if ui
                                    .checkbox(&mut is_null, "")
                                    .on_hover_text("NULL")
                                    .changed()
                                {
                                    self.bind_values.insert(key, (!is_null).then_some(text));
                                }
                                r
                            }
                            Variable::Substitution(subst) => {
                                let mut text = self.defines.get(subst).cloned().unwrap_or_default();
                                let field = egui::TextEdit::singleline(&mut text)
                                    .id_salt(("substitution", subst))
                                    .hint_text(hint);
                                let r = ui.add_sized([value_width, height], field);
                                if r.changed() {
                                    self.defines.insert(subst.clone(), text);
                                }
                                r
                            }
                        };
                        if asked && std::mem::take(&mut focus) {
                            response.request_focus();
                            response.scroll_to_me(Some(egui::Align::Center));
                        }
                        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            enter = true;
                        }
                    });
                }
            });
        if enter && self.state.prompt.is_some() {
            decision = Some(true);
        }
        match decision {
            Some(true) => self.run_prompted(cx),
            Some(false) => {
                self.cancel_waiting();
                Effects::default()
            }
            None => Effects::default(),
        }
    }

    /// Runs (or debugs) the waiting statement. Variables still without a
    /// value run empty, and keep that value.
    fn run_prompted(&mut self, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        let Some(prompt) = self.state.prompt.take() else {
            return effects;
        };
        for v in self.statement_variables(&prompt.statement.sql) {
            match v {
                Variable::Bind(name) => {
                    self.bind_values
                        .entry(name.to_lowercase())
                        .or_insert(Some(String::new()));
                }
                Variable::Substitution(name) => {
                    self.defines.entry(name).or_default();
                }
            }
        }
        let (sql, binds) = self.with_values(&prompt.statement.sql);
        if std::mem::take(&mut self.state.prompt_for_debug) {
            self.state.debug_start = Some(self.line_of(prompt.statement.start));
            effects.debug = Some(inline_binds(&sql, &binds));
        } else {
            self.send(prompt.statement.start, sql, binds, cx, &mut effects);
        }
        effects
    }

    /// Summary for the status bar of the active result, if one is shown.
    pub fn result_status(&self) -> Option<String> {
        let BottomTab::Result(i) = self.state.bottom else {
            return None;
        };
        let r = self.state.results.get(i)?;
        let more = if r.more { "+" } else { "" };
        Some(format!(
            "{}{more} rows · {}",
            r.rows.len(),
            fmt_duration(r.elapsed)
        ))
    }
}

/// Session statistics next to the actual execution plan.
fn statistics_ui(
    ui: &mut Ui,
    report: &crate::statistics::Report,
    view: &mut pythia_ui::plan::PlanView,
    text: &mut bool,
) {
    ui.label(
        RichText::new(format!(
            "{} rows fetched in {}",
            report.rows,
            fmt_duration(report.elapsed)
        ))
        .strong(),
    );
    ui.add_space(4.0);
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(300.0);
            match &report.stats {
                Ok(stats) => {
                    egui::Grid::new("stats")
                        .striped(true)
                        .num_columns(2)
                        .show(ui, |ui| {
                            for (name, value) in stats {
                                ui.label(name);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(RichText::new(value.to_string()).monospace());
                                    },
                                );
                                ui.end_row();
                            }
                        });
                }
                Err(e) => {
                    ui.colored_label(ui.visuals().warn_fg_color, e.as_str());
                }
            }
        });
        ui.separator();
        ui.vertical(|ui| {
            if let Ok(plan) = &report.plan
                && !report.plan_rows.is_empty()
            {
                ui.horizontal(|ui| {
                    ui.selectable_value(text, false, "Plan");
                    ui.selectable_value(text, true, "DBMS_XPLAN")
                        .on_hover_text("The plan as DBMS_XPLAN.DISPLAY_CURSOR shows it");
                    if let Some(worst) = pythia_ui::plan::worst_misestimate(&report.plan_rows) {
                        let note = format!(
                            "⚠ Estimate off at {} {}: {}",
                            worst.id,
                            worst.operation,
                            worst.misestimate_text().unwrap_or_default()
                        );
                        if ui
                            .add(
                                egui::Label::new(
                                    RichText::new(note).color(ui.visuals().warn_fg_color),
                                )
                                .truncate()
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text("Show the line")
                            .clicked()
                        {
                            *text = false;
                            view.select(Some(worst.id));
                        }
                    }
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(plan.clone());
                    }
                });
                if !*text {
                    view.ui(ui, &report.plan_rows, egui::Id::new("statistics-plan"));
                    return;
                }
            }
            egui::ScrollArea::both()
                .id_salt("xplan")
                .auto_shrink(false)
                .show(ui, |ui| match &report.plan {
                    Ok(plan) => {
                        ui.add(egui::Label::new(RichText::new(plan).monospace()).extend());
                    }
                    Err(e) => {
                        ui.colored_label(ui.visuals().warn_fg_color, e.as_str());
                    }
                });
        });
    });
}

/// Why *Edit* and *Apply* do nothing on a read-only connection.
const READ_ONLY_EDIT: &str = "Connection is read-only: results can't be edited";

/// Read-only connections only accept queries.
pub(crate) fn is_query(sql: &str) -> bool {
    let first = pythia_plsql::tokenize(sql)
        .into_iter()
        .find(|t| !t.kind.is_trivia());
    first.is_some_and(|t| {
        let word = &sql[t.span];
        ["SELECT", "WITH"]
            .iter()
            .any(|k| word.eq_ignore_ascii_case(k))
            || word == "("
    })
}

const DESCRIBE_SQL: &str = "select column_name as \"Name\", \
        decode(nullable, 'N', 'NOT NULL') as \"Null?\", \
        data_type || case \
          when data_type in ('VARCHAR2', 'NVARCHAR2', 'CHAR', 'NCHAR', 'RAW') \
            then '(' || decode(char_used, 'C', char_length || ' CHAR', data_length) || ')' \
          when data_precision is not null \
            then '(' || data_precision || nvl2(nullif(data_scale, 0), ',' || data_scale, '') || ')' \
        end as \"Type\" \
   from all_tab_columns \
  where table_name = upper(:name) \
    and owner = nvl(upper(:owner), sys_context('USERENV', 'CURRENT_SCHEMA')) \
  order by column_id";

enum EditAction {
    Start(usize),
    Apply(usize),
    Discard(usize),
    Done(usize),
    ShowSql(usize),
}

enum SqlPlus {
    Done,
    Run(String),
    Query(String, Vec<(String, Option<String>)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitution {
    pub range: Range<usize>,
    /// Lowercase name.
    pub name: String,
    pub double: bool,
}

/// Finds `&var` / `&&var` references, including inside string literals (as
/// SQL*Plus does), but not inside comments.
pub fn find_substitutions(sql: &str) -> Vec<Substitution> {
    let mut out = Vec::new();
    for token in tokenize(sql) {
        if !matches!(
            token.kind,
            TokenKind::SubstVar | TokenKind::String | TokenKind::QString
        ) {
            continue;
        }
        let text = &sql[token.span.clone()];
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'&' {
                i += 1;
                continue;
            }
            let start = i;
            let double = bytes.get(i + 1) == Some(&b'&');
            let name_start = i + if double { 2 } else { 1 };
            let name_len = text[name_start..]
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '#'))
                .unwrap_or(text.len() - name_start);
            if name_len == 0 {
                i = name_start;
                continue;
            }
            let mut end = name_start + name_len;
            // A trailing period terminates the name and is removed.
            if bytes.get(end) == Some(&b'.') {
                end += 1;
            }
            out.push(Substitution {
                range: token.span.start + start..token.span.start + end,
                name: text[name_start..name_start + name_len].to_lowercase(),
                double,
            });
            i = end;
        }
    }
    out
}

/// Replaces substitution variables that have a value.
pub fn substitute(sql: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut pos = 0;
    for s in find_substitutions(sql) {
        if let Some(value) = values.get(&s.name) {
            out.push_str(&sql[pos..s.range.start]);
            out.push_str(value);
            pos = s.range.end;
        }
    }
    out.push_str(&sql[pos..]);
    out
}

/// The line (1-based, within the block) where the debugged block itself is
/// stopped: the block is the outermost frame.
fn block_line(debug: &pythia_debugger::ui::DebugSession) -> Option<u32> {
    let frame = debug.current_frame()?;
    let block = &debug.stack.last()?.location.unit;
    (frame.location.unit.unit_type == pythia_debugger::UnitType::Block
        && &frame.location.unit == block)
        .then_some(frame.location.line)?
}

/// The debugger runs anonymous blocks, not SQL or stored unit DDL.
fn is_debuggable(stmt: &PendingStatement) -> bool {
    stmt.kind == StatementKind::PlSql && pythia_plsql::unit_header(&stmt.sql).is_none()
}

/// Replaces `:bind` references with literals (for debug runs, which execute
/// the block as plain text).
pub fn inline_binds(sql: &str, binds: &[(String, Option<String>)]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut pos = 0;
    for t in tokenize(sql)
        .into_iter()
        .filter(|t| t.kind == TokenKind::BindVar)
    {
        let name = &sql[t.span.start + 1..t.span.end];
        let Some((_, value)) = binds.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) else {
            continue;
        };
        out.push_str(&sql[pos..t.span.start]);
        match value {
            None => out.push_str("NULL"),
            Some(v) if v.trim().parse::<f64>().is_ok() => out.push_str(v.trim()),
            Some(v) => out.push_str(&format!("'{}'", v.replace('\'', "''"))),
        }
        pos = t.span.end;
    }
    out.push_str(&sql[pos..]);
    out
}

/// Names of `:bind` variables, excluding trigger `:new`/`:old` references.
pub fn find_binds(sql: &str) -> Vec<String> {
    if pythia_plsql::unit_header(sql).is_some() {
        // Stored units (incl. triggers) don't take binds when created.
        return Vec::new();
    }
    tokenize(sql)
        .into_iter()
        .filter(|t| t.kind == TokenKind::BindVar)
        .map(|t| sql[t.span.start + 1..t.span.end].to_owned())
        .filter(|name| !name.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

/// SQL*Plus commands the SQL window carries out itself for any database
/// (scripts, prompts, substitution variables). The others mean something
/// else to other databases and go to the server.
pub(crate) fn client_command(sql: &str) -> bool {
    let line = sql.trim_start();
    let command = line
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    line.starts_with('@')
        || matches!(
            command.as_str(),
            "PRO"
                | "PROMPT"
                | "DEF"
                | "DEFINE"
                | "UNDEF"
                | "UNDEFINE"
                | "REM"
                | "REMARK"
                | "WHENEVER"
        )
}

/// Width of a line of text as `ui` lays it out.
fn text_width(ui: &Ui, text: RichText) -> f32 {
    egui::WidgetText::from(text)
        .into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Body,
        )
        .size()
        .x
}

/// Binds are the same in any case.
fn same_variable(a: &Variable, b: &Variable) -> bool {
    match (a, b) {
        (Variable::Bind(a), Variable::Bind(b)) => a.eq_ignore_ascii_case(b),
        _ => a == b,
    }
}

/// Removes case-insensitive duplicates, keeping the first spelling.
fn dedup(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if !out.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    }
    out
}

fn first_line(sql: &str) -> String {
    let line = sql.lines().next().unwrap_or_default();
    if line.chars().count() > 70 {
        format!("{}…", line.chars().take(70).collect::<String>())
    } else {
        line.to_owned()
    }
}

/// Asks for a file name and writes the export on a helper thread, so the
/// native dialog doesn't block rendering.
fn export_in_background(
    format: pythia_data::Format,
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
    dialect: pythia_db::Dialect,
) -> std::sync::mpsc::Receiver<Result<Option<std::path::PathBuf>, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let path = rfd::FileDialog::new()
            .set_file_name(format!("result.{}", format.extension()))
            .add_filter(format.label(), &[format.extension()])
            .save_file();
        let result = match path {
            None => Ok(None),
            Some(path) => std::fs::File::create(&path)
                .map_err(|e| e.to_string())
                .and_then(|file| {
                    let mut out = std::io::BufWriter::new(file);
                    pythia_data::export(
                        format,
                        &columns,
                        &rows,
                        &pythia_data::Options {
                            dialect,
                            ..Default::default()
                        },
                        &mut out,
                    )
                    .map_err(|e| e.to_string())?;
                    std::io::Write::flush(&mut out).map_err(|e| e.to_string())
                })
                .map(|()| Some(path)),
        };
        let _ = tx.send(result);
    });
    rx
}

pub fn fmt_duration(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

/// An error with a PL/SQL stack, line by line: its frames are links to their
/// code. The frame clicked, if any; clicking the first line of an error with
/// a position in the statement sets `jump`.
fn stack_message_ui(
    ui: &mut Ui,
    m: &Message,
    color: egui::Color32,
    jump: &mut Option<Range<usize>>,
) -> Option<FrameTarget> {
    let mut open = None;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (i, line) in m.text.lines().enumerate() {
            let text = RichText::new(line).monospace().color(color);
            match m.stack.iter().find(|f| f.text_line == i).map(|f| &f.target) {
                Some(target) if target.opens() => {
                    if ui
                        .link(text)
                        .on_hover_ui(|ui| frame_hint(ui, target))
                        .clicked()
                    {
                        open = Some(target.clone());
                    }
                }
                Some(FrameTarget::Hidden(why)) => {
                    ui.label(text).on_hover_text(why);
                }
                _ => match &m.jump {
                    Some(range) if i == 0 => {
                        if ui.link(text).on_hover_text("Go to error").clicked() {
                            *jump = Some(range.clone());
                        }
                    }
                    _ => {
                        ui.label(text);
                    }
                },
            }
        }
    });
    open
}

/// Where a frame's link leads, and the line of code there.
fn frame_hint(ui: &mut Ui, target: &FrameTarget) {
    let code = match target {
        FrameTarget::Program { object, line, text } => {
            ui.label(format!(
                "Open {} ({}) at line {line}",
                object.qualified(),
                object.object_type.dictionary_name().to_lowercase()
            ));
            text
        }
        FrameTarget::Statement { text, .. } => {
            ui.label("Go to this line of the statement");
            text
        }
        FrameTarget::Pending(_) | FrameTarget::Hidden(_) => return,
    };
    if !code.trim().is_empty() {
        ui.label(RichText::new(code.trim()).monospace());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a read-only connection, nothing makes a result editable or
    /// applies its changes (#107).
    #[test]
    fn read_only_connections_dont_edit_results() {
        let settings = pythia_config::Settings::default();
        let cx = |read_only| DocContext {
            id: crate::documents::DocId(1),
            settings: &settings,
            connected: true,
            fetch_size: 100,
            connection: Some("shop".into()),
            catalog: None,
            catalog_revision: 0,
            connections: &[],
            debug: None,
            marks: None,
            privilege: None,
            dialect: pythia_db::Dialect::MYSQL,
            read_only,
            production: false,
        };
        let column = |name: &str| Column {
            name: name.into(),
            type_name: "INT".into(),
            numeric: true,
        };
        let mut w = SqlWindow::new("t".into(), String::new());
        w.state.running = Some(Running {
            start: 0,
            sql: "select x from t".into(),
            internal: None,
        });
        w.on_event(
            DbEvent::QueryStarted {
                id: 7,
                columns: vec![column("x"), column("pythia$id")],
            },
            &cx(false),
        );
        w.on_event(
            DbEvent::Rows {
                id: 7,
                rows: vec![vec![Some("1".into()), Some("1".into())]],
                more: false,
                elapsed: Duration::ZERO,
            },
            &cx(false),
        );
        assert!(w.start_editing(0, &cx(true)).requests.is_empty());
        assert!(w.state.queue.is_empty() && w.state.running.is_none());

        // Made editable on a writable connection, with a change pending.
        let r = &mut w.state.results[0];
        r.edit = Some(crate::data_edit::Editable {
            table: "t".into(),
            source: "select x from t".into(),
            key: crate::data_edit::RowKey::Columns(vec!["id".into()]),
        });
        r.grid.set_editable(Some(1));
        let rows = r.rows.clone();
        r.grid.edit_cell(&rows, 0, 0, Some("2".into()));
        assert!(w.apply_changes(0, &cx(true)).requests.is_empty());
        assert!(w.state.queue.is_empty() && w.state.running.is_none());
        let texts: Vec<&str> = w.state.messages.iter().map(|m| m.text.as_str()).collect();
        assert!(texts.contains(&READ_ONLY_EDIT), "{texts:?}");
        assert!(
            w.state.results[0]
                .grid
                .changes()
                .is_some_and(|c| !c.is_empty()),
            "the change stays to apply elsewhere"
        );

        let sent = w.apply_changes(0, &cx(false)).requests;
        assert!(
            matches!(&sent[..], [(_, DbRequest::Execute { sql, .. })]
                if sql == "update t set x = 2 where id = 1 and x = 1"),
            "{sent:?}"
        );
    }

    #[test]
    fn client_commands() {
        for local in [
            "@setup",
            "@@next.sql",
            "prompt hello",
            "define x = 1",
            "rem note",
        ] {
            assert!(client_command(local), "{local}");
        }
        for server in [
            "set @a = 1",
            "show tables",
            "desc emp",
            "start transaction",
            "exec x",
        ] {
            assert!(!client_command(server), "{server}");
        }
    }

    #[test]
    fn substitutions() {
        let sql = "select '&a', &&b. || x from t where c = &c -- &d\n/* &e */";
        let names: Vec<_> = find_substitutions(sql)
            .into_iter()
            .map(|s| (s.name, s.double))
            .collect();
        assert_eq!(
            names,
            [("a".into(), false), ("b".into(), true), ("c".into(), false)]
        );
        let values = BTreeMap::from([
            ("a".into(), "1".into()),
            ("b".into(), "2".into()),
            ("c".into(), "3".into()),
        ]);
        assert_eq!(
            substitute(sql, &values),
            "select '1', 2 || x from t where c = 3 -- &d\n/* &e */"
        );
    }

    #[test]
    fn binds() {
        assert_eq!(
            find_binds("select :a, :B, :a from t where x = :1 and y = ':z'"),
            ["a", "B", "a"]
        );
        assert!(
            find_binds("create or replace trigger t before insert on x for each row begin :new.id := 1; end;")
                .is_empty()
        );
        assert_eq!(
            dedup(["a".to_owned(), "A".to_owned(), "b".to_owned()].into_iter()),
            ["a", "b"]
        );
    }

    #[test]
    fn inlines_binds() {
        let binds = vec![
            ("a".into(), Some("12".into())),
            ("B".into(), Some("O'Neil".into())),
            ("c".into(), None),
        ];
        assert_eq!(
            inline_binds("p(:a, :b, :c, ':a')", &binds),
            "p(12, 'O''Neil', NULL, ':a')"
        );
    }

    #[test]
    fn legacy_files_keep_encoding_and_line_endings() {
        let dir = std::env::temp_dir().join(format!("pythia-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("alt.sql");
        let mut w = SqlWindow::from_file(
            path.clone(),
            crate::text_file::decode(b"-- K\xe4se\r\nselect 1 from dual;\r\n"),
        );
        assert_eq!(w.editor.text(), "-- Käse\nselect 1 from dual;\n");
        w.editor.select(16..17);
        w.editor.insert("2");
        w.save_to(path.clone()).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"-- K\xe4se\r\nselect 2 from dual;\r\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_roundtrip_and_dirty() {
        let dir = std::env::temp_dir().join(format!("pythia-sqlfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("q.sql");
        let mut w = SqlWindow::from_file(
            path.clone(),
            crate::text_file::decode(b"select 1 from dual"),
        );
        assert_eq!(w.title, "q.sql");
        assert!(!w.is_dirty());
        w.editor.select(7..8);
        w.editor.insert("2");
        assert!(w.is_dirty());
        w.save_to(path.clone()).unwrap();
        assert!(!w.is_dirty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "select 2 from dual"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn debuggable_follows_cursor_and_selection() {
        let text = "select 1 from dual;\nbegin\n  null;\nend;\n/\ncreate procedure p as begin null; end;\n/\n";
        let mut w = SqlWindow::new("SQL 1".into(), text.into());
        let mut at = |range: Range<usize>| {
            w.editor.select(range);
            let changed = w.refresh_debuggable();
            (w.debuggable(), changed)
        };
        let pos = |needle: &str| text.find(needle).unwrap();
        // Not debuggable until worked out, so this isn't a change.
        assert_eq!(at(pos("select")..pos("select")), (false, false));
        assert_eq!(at(pos("null")..pos("null")), (true, true));
        // Another place in the same block: same answer.
        assert_eq!(at(pos("end;")..pos("end;")), (true, false));
        assert_eq!(at(pos("create")..pos("create")), (false, true));
        // A selection is debugged from its first statement.
        assert_eq!(at(pos("begin")..text.len()), (true, true));
        assert_eq!(at(0..text.len()), (false, true));
    }

    /// An error with a PL/SQL stack: its frames are looked up and lead to
    /// the code, one click away (#112).
    #[test]
    fn error_stack_frames_open_their_code() {
        use egui_kittest::kittest::Queryable;
        use pythia_db::ObjectType;
        use pythia_db::error_stack::FrameSource;
        fn context(settings: &pythia_config::Settings) -> DocContext<'_> {
            DocContext {
                id: crate::documents::DocId(1),
                settings,
                connected: true,
                fetch_size: 100,
                connection: Some("db".into()),
                catalog: None,
                catalog_revision: 0,
                connections: &[],
                debug: None,
                marks: None,
                privilege: None,
                dialect: pythia_db::Dialect::ORACLE,
                read_only: false,
                production: false,
            }
        }
        let settings = pythia_config::Settings::default();
        let cx = context(&settings);
        let text = "select 1 from dual;\nbegin\n  shop.orders.place(1);\nend;";
        let start = text.find("begin").unwrap();
        let mut w = SqlWindow::new("SQL 1".into(), text.into());
        w.state.running = Some(Running {
            start,
            sql: text[start..].into(),
            internal: None,
        });
        let error = DbError {
            code: Some(1403),
            oracle: true,
            ..DbError::new(
                "ORA-01403: no data found\n\
                 ORA-06512: at \"SYS.DBMS_ASSERT\", line 192\n\
                 ORA-06512: at \"SHOP.ORDERS\", line 40\n\
                 ORA-06512: at line 1\n\
                 ORA-06512: at line 2",
            )
        };
        let effects = w.on_event(
            DbEvent::Failed {
                id: 1,
                error,
                elapsed: Duration::ZERO,
            },
            &cx,
        );
        let [(Target::Main, DbRequest::Task { id, .. })] = effects.requests.as_slice() else {
            panic!("expected one lookup on the main session");
        };
        let id = *id;
        let message = &w.state.messages[0];
        let targets: Vec<_> = message
            .stack
            .iter()
            .map(|f| (f.text_line, f.target.clone()))
            .collect();
        // The block run by dynamic SQL (line 4) leads nowhere; the last
        // frame is the statement's second line.
        let call = text.find("shop.").unwrap();
        assert_eq!(
            targets,
            [
                (1, FrameTarget::Pending(id)),
                (2, FrameTarget::Pending(id)),
                (
                    4,
                    FrameTarget::Statement {
                        range: call..call + "shop.orders.place(1);".len(),
                        text: "shop.orders.place(1);".into()
                    }
                ),
            ]
        );

        let found: Vec<(String, String, FrameSource)> = vec![
            (
                "SYS".into(),
                "DBMS_ASSERT".into(),
                FrameSource::Hidden("The source of its package body isn't visible to you".into()),
            ),
            (
                "SHOP".into(),
                "ORDERS".into(),
                FrameSource::Source {
                    object_type: ObjectType::PackageBody,
                    line: 40,
                    text: "    select id into l_id from orders where id = p_id;".into(),
                },
            ),
        ];
        w.on_event(
            DbEvent::TaskDone {
                id,
                result: Ok(Box::new(found)),
            },
            &cx,
        );
        let orders = FrameTarget::Program {
            object: ObjectRef {
                owner: "SHOP".into(),
                name: "ORDERS".into(),
                object_type: ObjectType::PackageBody,
            },
            line: 40,
            text: "    select id into l_id from orders where id = p_id;".into(),
        };
        assert!(matches!(
            &w.state.messages[0].stack[0].target,
            FrameTarget::Hidden(_)
        ));
        assert_eq!(w.state.messages[0].stack[1].target, orders);

        // "Open where raised" opens the innermost frame whose code can be
        // shown, at its line.
        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = opened.clone();
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 600.0))
            .build_ui(move |ui| {
                let settings = pythia_config::Settings::default();
                let effects = w.ui(ui, &context(&settings));
                sink.borrow_mut().extend(effects.open);
            });
        h.run_steps(2);
        h.get_by_label("Open where raised").click();
        h.run_steps(2);
        let opened = opened.borrow();
        let [
            OpenRequest::ProgramAt {
                connection,
                object,
                line,
            },
        ] = opened.as_slice()
        else {
            panic!("expected the package body to open: {opened:?}");
        };
        assert_eq!(
            (connection.as_str(), object.name.as_str(), *line),
            ("db", "ORDERS", 40)
        );
        assert_eq!(object.object_type, ObjectType::PackageBody);
    }

    /// The panel lists the text's variables in fields wide enough to read
    /// (#109). A run waits for the one without a value, focused, and Enter
    /// in its field runs the statement with it (#110).
    #[test]
    fn variables_panel_runs_with_entered_values() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        struct State {
            w: SqlWindow,
            execute: bool,
            sent: Vec<(u64, String)>,
        }
        let settings = pythia_config::Settings::default();
        let mut w = SqlWindow::new(
            "SQL 1".into(),
            "select :email_address from t where x = &&tab".into(),
        );
        w.defines.insert("tab".into(), "1".into());
        let state = State {
            w,
            execute: false,
            sent: Vec::new(),
        };
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 600.0))
            .build_ui_state(
                move |ui, st: &mut State| {
                    let cx = test_cx(&settings, false);
                    let mut effects = Effects::default();
                    if std::mem::take(&mut st.execute) {
                        effects = st.w.execute(false, &cx);
                    }
                    effects.extend(st.w.ui(ui, &cx));
                    st.sent.extend(sent(&effects));
                },
                state,
            );
        h.run_steps(3);
        let fields: Vec<_> = h.get_all_by_role(Role::TextInput).collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].value().as_deref(), Some("1"));
        for field in &fields {
            assert!(field.rect().width() >= 150.0, "{:?}", field.rect());
        }
        drop(fields);

        h.state_mut().execute = true;
        h.run_steps(3);
        assert!(h.state().w.waiting_for_values());
        assert!(h.state().sent.is_empty());
        h.get_by_label("Waiting for values to run the statement at line 1:");
        let email = h.get_all_by_role(Role::TextInput).nth(1).unwrap();
        assert!(email.is_focused());
        email.type_text("a@b.c");
        h.run_steps(1);
        h.key_press(egui::Key::Enter);
        h.run_steps(2);
        let st = h.state();
        assert!(!st.w.waiting_for_values());
        assert_eq!(st.sent.len(), 1);
        assert_eq!(st.sent[0].1, "select :email_address from t where x = 1");
        assert_eq!(st.w.bind_values["email_address"], Some("a@b.c".into()));
    }

    /// A document context on an Oracle connection for tests.
    fn test_cx(settings: &pythia_config::Settings, read_only: bool) -> DocContext<'_> {
        DocContext {
            id: crate::documents::DocId(1),
            settings,
            connected: true,
            fetch_size: 100,
            connection: Some("db".into()),
            catalog: None,
            catalog_revision: 0,
            connections: &[],
            debug: None,
            marks: None,
            privilege: None,
            dialect: pythia_db::Dialect::ORACLE,
            read_only,
            production: false,
        }
    }

    /// Statements the effects send to the database.
    fn sent(effects: &Effects) -> Vec<(u64, String)> {
        effects
            .requests
            .iter()
            .filter_map(|(_, r)| match r {
                DbRequest::Execute { id, sql, .. } => Some((*id, sql.clone())),
                _ => None,
            })
            .collect()
    }

    /// On a read-only connection only queries reach the server: also the
    /// statements after the first of a script, and ones that asked for
    /// variables (#115).
    #[test]
    fn read_only_connections_run_only_queries() {
        let settings = pythia_config::Settings::default();
        let cx = test_cx(&settings, true);
        let mut w = SqlWindow::new("t".into(), "select 1 from dual;\ndelete from t;\n".into());
        let first = sent(&w.execute(false, &cx));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1, "select 1 from dual");
        let id = first[0].0;
        w.on_event(
            DbEvent::QueryStarted {
                id,
                columns: Vec::new(),
            },
            &cx,
        );
        let next = w.on_event(
            DbEvent::Rows {
                id,
                rows: Vec::new(),
                more: false,
                elapsed: Duration::ZERO,
            },
            &cx,
        );
        assert_eq!(sent(&next), []);
        assert!(!w.is_running());

        // With a variable, the statement waits for its value first.
        let mut w = SqlWindow::new("t".into(), "delete from t where id = :id".into());
        let mut effects = w.execute(false, &cx);
        assert!(w.state.prompt.is_some());
        effects.extend(w.run_prompted(&cx));
        assert_eq!(sent(&effects), []);
        assert!(!w.is_running());
        assert!(
            w.state
                .messages
                .iter()
                .any(|m| m.text.contains("read-only")),
            "{:?}",
            w.state.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
        );

        // Profile runs a block, which could change anything.
        let mut w = SqlWindow::new("t".into(), "begin\n  p;\nend;".into());
        assert!(w.profile(true).requests.is_empty());
        assert!(
            w.state
                .messages
                .iter()
                .any(|m| m.text.contains("read-only"))
        );
        assert_eq!(w.profile(false).requests.len(), 1);
    }

    /// Every variable of the text, once, whatever the selection; not the
    /// `:new` of a trigger.
    #[test]
    fn variables_of_the_whole_text() {
        let text = "select :a, &&t from dual;\nselect :A, :b, '&t' from x;\n\
                    create trigger tr before insert on x for each row begin :new.id := 1; end;\n/\n";
        let mut w = SqlWindow::new("t".into(), text.into());
        w.editor.select(0..10);
        assert_eq!(
            w.variables(),
            [
                Variable::Substitution("t".into()),
                Variable::Bind("a".into()),
                Variable::Bind("b".into()),
            ]
        );
        w.substitution = false;
        assert_eq!(
            w.variables(),
            [Variable::Bind("a".into()), Variable::Bind("b".into())]
        );
        w.editor.set_text("select 1 from dual");
        assert!(w.variables().is_empty());
    }

    /// Statements run with the values the window has; one with a variable
    /// that has none waits, and Execute again runs it (#110).
    #[test]
    fn runs_with_stored_values_and_waits_for_missing_ones() {
        let settings = pythia_config::Settings::default();
        let cx = test_cx(&settings, false);
        let mut w = SqlWindow::new(
            "t".into(),
            "select 1 from dual;\nselect :a, :b from t where x = &x;\n".into(),
        );
        w.bind_values.insert("a".into(), None);
        let first = sent(&w.execute(false, &cx));
        assert_eq!(first.len(), 1);
        let id = first[0].0;
        w.on_event(
            DbEvent::QueryStarted {
                id,
                columns: Vec::new(),
            },
            &cx,
        );
        let done = DbEvent::Rows {
            id,
            rows: Vec::new(),
            more: false,
            elapsed: Duration::ZERO,
        };
        // :b and &x have no value: the second statement waits.
        assert_eq!(sent(&w.on_event(done, &cx)), []);
        assert!(w.waiting_for_values() && w.is_running());
        w.defines.insert("x".into(), "3".into());
        let effects = w.execute(false, &cx);
        let Some((_, DbRequest::Execute { sql, binds, .. })) = effects.requests.first() else {
            panic!("not run");
        };
        assert_eq!(sql, "select :a, :b from t where x = 3");
        // :a is NULL; :b ran empty and keeps that.
        assert_eq!(
            binds,
            &[
                ("a".to_owned(), None),
                ("b".to_owned(), Some(String::new()))
            ]
        );
        assert_eq!(w.bind_values["b"], Some(String::new()));
        assert!(!w.waiting_for_values());

        // Values known: no waiting.
        let mut w = SqlWindow::new("t".into(), "select :a from dual".into());
        w.bind_values.insert("A".to_lowercase(), Some("7".into()));
        let effects = w.execute(false, &cx);
        assert_eq!(sent(&effects).len(), 1);

        // Cancel drops the statement and the rest of the script.
        let mut w = SqlWindow::new(
            "t".into(),
            "select :z from dual;\nselect 2 from dual;\n".into(),
        );
        w.execute(false, &cx);
        assert!(w.waiting_for_values());
        w.cancel_waiting();
        assert!(!w.is_running() && w.state.queue.is_empty());
    }

    /// Debug waits for values too, and runs the block with them inlined.
    #[test]
    fn debug_waits_for_values() {
        let settings = pythia_config::Settings::default();
        let cx = test_cx(&settings, false);
        let mut w = SqlWindow::new("t".into(), "begin\n  p(:n, :s);\nend;".into());
        w.bind_values.insert("s".into(), Some("x".into()));
        assert!(w.debug_current().debug.is_none());
        assert!(w.waiting_for_values());
        w.bind_values.insert("n".into(), Some("5".into()));
        let effects = w.execute(false, &cx);
        assert_eq!(effects.debug.as_deref(), Some("begin\n  p(5, 'x');\nend;"));
        assert!(effects.requests.is_empty());
        // Known values: straight to the debugger.
        assert!(w.debug_current().debug.is_some());
    }
}
