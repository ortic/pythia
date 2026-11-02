//! Project panel: a folder of SQL and PL/SQL files with git status, diffs
//! against the last commit or the database, schema export and deployment of
//! changed files.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use egui::{Color32, RichText, Sense, Ui};
use pythia_db::{DbRequest, Dialect, Feature};
use pythia_ui::icons::{Icon, IconSize, icon_text_button};
use serde::{Deserialize, Serialize};

use crate::documents::request_id;
use crate::git::{FileStatus, Repo};
use crate::object_files::{ExportProgress, object_of_file, object_text};

/// File types the editor opens; others are listed but dimmed.
const SQL_EXTENSIONS: &[&str] = &[
    "sql", "pks", "pkb", "pck", "prc", "fnc", "trg", "tps", "tpb", "vw", "ddl", "dml", "plb",
];

/// Folders never shown.
const HIDDEN_DIRS: &[&str] = &[".git", "target", "node_modules", ".idea", ".vscode"];

pub fn is_sql_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SQL_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[derive(Default, Serialize, Deserialize)]
pub struct Project {
    #[serde(default, with = "crate::serde_path::option")]
    pub root: Option<PathBuf>,
    expanded: BTreeSet<PathBuf>,
    /// Show only changed files, as a flat list.
    changes_only: bool,
    #[serde(skip)]
    state: State,
}

#[derive(Default)]
struct State {
    /// The connection's database: whether files can be compared with it
    /// and its schema exported.
    dialect: Dialect,
    repo: Option<Repo>,
    /// Directory listings, cleared on refresh.
    listings: HashMap<PathBuf, Vec<Entry>>,
    refreshed: Option<f64>,
    selected: Option<PathBuf>,
    pick: Option<(Receiver<Option<PathBuf>>, Pick)>,
    export: Option<Export>,
    /// Export folder chosen; starts once the connection is known.
    pending_export: Option<PathBuf>,
    compare: Option<Compare>,
    message: Option<(String, bool)>,
}

#[derive(Clone, Copy)]
enum Pick {
    Root,
    ExportTo,
}

struct Export {
    dir: PathBuf,
    progress: Arc<ExportProgress>,
    result: Arc<Mutex<Option<Result<usize, String>>>>,
}

/// Label and text of a database object, or an error.
type DbText = Result<(String, String), String>;

/// A file compared with its database object; the task fills `result`.
struct Compare {
    file: PathBuf,
    connection: String,
    result: Arc<Mutex<Option<DbText>>>,
}

#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    dir: bool,
}

/// What the app should do for the panel.
pub enum ProjectAction {
    Open(PathBuf),
    /// Open a diff document.
    Diff {
        title: String,
        left: (String, String),
        right: (String, String),
        file: PathBuf,
    },
    /// Open a SQL window with a deployment script.
    Deploy(String),
    /// Run a task on a connection's main session.
    Request(String, DbRequest),
}

impl Project {
    #[cfg(test)]
    pub fn open_root(&mut self, root: PathBuf) {
        self.root = Some(root);
        self.state = State::default();
    }

    #[cfg(test)]
    pub fn is_busy(&self) -> bool {
        self.state.export.is_some() || self.state.compare.is_some()
    }

    #[cfg(test)]
    pub fn export_progress(&self) -> Option<(usize, usize)> {
        self.state.export.as_ref().map(|e| {
            (
                e.progress.done.load(Ordering::Relaxed),
                e.progress.total.load(Ordering::Relaxed),
            )
        })
    }

    #[cfg(test)]
    pub fn message(&self) -> Option<&str> {
        self.state.message.as_ref().map(|(m, _)| m.as_str())
    }

    #[cfg(test)]
    pub fn refresh_now(&mut self) {
        self.refresh(0.0);
        self.state.refreshed = None;
    }

    #[cfg(test)]
    pub fn repo(&self) -> Option<&Repo> {
        self.state.repo.as_ref()
    }

    #[cfg(test)]
    pub fn expand(&mut self, dir: PathBuf) {
        self.expanded.insert(dir);
    }

    fn refresh(&mut self, now: f64) {
        self.state.listings.clear();
        self.state.repo = self.root.as_deref().and_then(crate::git::open);
        self.state.refreshed = Some(now);
    }

    fn listing(&mut self, dir: &Path) -> &[Entry] {
        self.state
            .listings
            .entry(dir.to_owned())
            .or_insert_with(|| list_dir(dir))
    }

    fn status(&self, path: &Path) -> Option<FileStatus> {
        self.state.repo.as_ref()?.status.get(path).copied()
    }

    /// Changed SQL files under the project folder (not deleted ones).
    fn changed_files(&self) -> Vec<(PathBuf, FileStatus)> {
        let (Some(root), Some(repo)) = (&self.root, &self.state.repo) else {
            return Vec::new();
        };
        let mut files: Vec<(PathBuf, FileStatus)> = repo
            .status
            .iter()
            .filter(|(p, s)| p.starts_with(root) && **s != FileStatus::Deleted)
            .map(|(p, s)| (p.clone(), *s))
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        connection: Option<&str>,
        dialect: Dialect,
    ) -> Vec<ProjectAction> {
        self.state.dialect = dialect;
        let mut actions = Vec::new();
        let now = ui.input(|i| i.time);
        self.poll(ui, &mut actions);
        if let Some(dir) = self.state.pending_export.take()
            && let Some(c) = connection
        {
            self.export_schema(dir, c, &mut actions);
        }
        let Some(root) = self.root.clone() else {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No project folder").strong());
                ui.weak("Open a folder with your SQL and PL/SQL files.");
                ui.add_space(8.0);
                if icon_text_button(ui, Icon::FolderOpen, "Open folder…").clicked() {
                    self.pick(Pick::Root);
                }
            });
            return actions;
        };
        // Git status is cheap; keep it current while the panel is visible.
        if self.state.refreshed.is_none_or(|t| now - t > 5.0) {
            self.refresh(now);
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(5));

        ui.horizontal(|ui| {
            ui.add(Icon::FolderOpen.sized(ui, IconSize::Small));
            let name = root.file_name().map_or_else(
                || root.display().to_string(),
                |n| n.to_string_lossy().into(),
            );
            ui.label(RichText::new(name).strong())
                .on_hover_text(root.display().to_string());
            if let Some(repo) = &self.state.repo {
                ui.weak(format!("on {}", repo.branch));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("⋯", |ui| {
                    if ui.button("Open another folder…").clicked() {
                        self.pick(Pick::Root);
                        ui.close();
                    }
                    if dialect.supports(Feature::ObjectFiles)
                        && ui
                            .add_enabled(
                                connection.is_some() && self.state.export.is_none(),
                                egui::Button::new("Export schema to folder…"),
                            )
                            .on_hover_text("One file per object: tables/, packages/, …")
                            .clicked()
                    {
                        self.pick(Pick::ExportTo);
                        ui.close();
                    }
                    if ui.button("Close project").clicked() {
                        self.root = None;
                        self.state = State::default();
                        ui.close();
                    }
                });
                if ui
                    .add(
                        Icon::Refresh
                            .sized(ui, IconSize::Small)
                            .sense(Sense::click()),
                    )
                    .on_hover_text("Refresh")
                    .clicked()
                {
                    self.refresh(now);
                }
            });
        });
        let changed = self.changed_files();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.changes_only, false, "Files");
            let label = if changed.is_empty() {
                "Changes".to_owned()
            } else {
                format!("Changes ({})", changed.len())
            };
            ui.selectable_value(&mut self.changes_only, true, label);
            let deployable: Vec<&PathBuf> = changed
                .iter()
                .map(|(p, _)| p)
                .filter(|p| is_sql_file(p))
                .collect();
            if !deployable.is_empty()
                && ui
                    .small_button("Deploy…")
                    .on_hover_text("Open a script running the changed files with @")
                    .clicked()
            {
                actions.push(ProjectAction::Deploy(deploy_script(&root, &deployable)));
            }
        });
        if let Some(export) = &self.state.export {
            let done = export.progress.done.load(Ordering::Relaxed);
            let total = export.progress.total.load(Ordering::Relaxed);
            ui.horizontal(|ui| {
                ui.spinner();
                let fraction = if total == 0 {
                    0.0
                } else {
                    done as f32 / total as f32
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(160.0)
                        .text(format!("Exporting {done} / {total}")),
                );
                if ui.small_button("Stop").clicked() {
                    export.progress.stop.store(true, Ordering::Relaxed);
                }
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
        if let Some(compare) = &self.state.compare {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(format!(
                    "Reading {} from {}…",
                    file_name(&compare.file),
                    compare.connection
                ));
            });
        }
        if let Some((text, error)) = &self.state.message {
            let color = if *error {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().weak_text_color()
            };
            ui.label(RichText::new(text).color(color).small());
        }
        ui.separator();

        let mut rows: Vec<(usize, Entry)> = Vec::new();
        if self.changes_only {
            for (path, _) in &changed {
                let name = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                rows.push((
                    0,
                    Entry {
                        path: path.clone(),
                        name,
                        dir: false,
                    },
                ));
            }
        } else {
            self.collect(&root, 0, &mut rows);
        }
        if rows.is_empty() {
            ui.weak(if self.changes_only {
                "No changes"
            } else {
                "Empty folder"
            });
        }
        let row_height = pythia_ui::row_height(ui);
        let mut toggles = Vec::new();
        egui::ScrollArea::both().auto_shrink(false).show_rows(
            ui,
            row_height,
            rows.len(),
            |ui, range| {
                for (depth, entry) in &rows[range] {
                    self.row_ui(
                        ui,
                        *depth,
                        entry,
                        row_height,
                        connection,
                        &mut toggles,
                        &mut actions,
                    );
                }
            },
        );
        for dir in toggles {
            if !self.expanded.remove(&dir) {
                self.expanded.insert(dir);
            }
        }
        actions
    }

    fn collect(&mut self, dir: &Path, depth: usize, rows: &mut Vec<(usize, Entry)>) {
        let entries = self.listing(dir).to_vec();
        for entry in entries {
            let open = entry.dir && self.expanded.contains(&entry.path);
            let path = entry.path.clone();
            rows.push((depth, entry));
            if open {
                self.collect(&path, depth + 1, rows);
            }
        }
    }

    #[expect(clippy::too_many_arguments, reason = "one row of the tree")]
    fn row_ui(
        &mut self,
        ui: &mut Ui,
        depth: usize,
        entry: &Entry,
        height: f32,
        connection: Option<&str>,
        toggles: &mut Vec<PathBuf>,
        actions: &mut Vec<ProjectAction>,
    ) {
        let small = IconSize::Small;
        let status = if entry.dir {
            self.dir_status(&entry.path)
        } else {
            self.status(&entry.path)
        };
        ui.horizontal(|ui| {
            ui.set_height(height);
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.add_space(depth as f32 * 16.0);
            let sql = is_sql_file(&entry.path);
            if entry.dir {
                let open = self.expanded.contains(&entry.path);
                let chevron = if open {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                };
                let color = ui.visuals().weak_text_color();
                let r = ui.add(
                    chevron
                        .mono(color)
                        .fit_to_exact_size(egui::vec2(small.points(), small.points()))
                        .sense(Sense::click()),
                );
                if r.clicked() {
                    toggles.push(entry.path.clone());
                }
                ui.add(if open { Icon::FolderOpen } else { Icon::Folder }.sized(ui, small));
            } else {
                ui.add_space(small.points());
                let icon = file_icon(&entry.path);
                ui.add(icon.sized(ui, small));
            }
            let mut text = RichText::new(&entry.name);
            if let Some(s) = status {
                text = text.color(status_color(ui, s));
            } else if !entry.dir && !sql {
                text = text.color(ui.visuals().weak_text_color());
            }
            let selected = self.state.selected.as_ref() == Some(&entry.path);
            let response = ui.add(egui::Button::selectable(selected, text));
            if let Some(s) = status
                && !entry.dir
            {
                ui.label(
                    RichText::new(s.letter())
                        .small()
                        .strong()
                        .color(status_color(ui, s)),
                )
                .on_hover_text(s.label());
            }
            if response.clicked() {
                self.state.selected = Some(entry.path.clone());
                if entry.dir {
                    toggles.push(entry.path.clone());
                }
            }
            if response.double_clicked() && !entry.dir && sql {
                actions.push(ProjectAction::Open(entry.path.clone()));
            }
            if !entry.dir {
                response.context_menu(|ui| {
                    self.file_menu(ui, &entry.path, status, connection, actions)
                });
            }
        });
    }

    fn file_menu(
        &mut self,
        ui: &mut Ui,
        path: &Path,
        status: Option<FileStatus>,
        connection: Option<&str>,
        actions: &mut Vec<ProjectAction>,
    ) {
        let sql = is_sql_file(path);
        if ui.add_enabled(sql, egui::Button::new("Open")).clicked() {
            actions.push(ProjectAction::Open(path.to_owned()));
            ui.close();
        }
        let committed = matches!(status, Some(FileStatus::Modified | FileStatus::Renamed));
        if ui
            .add_enabled(committed, egui::Button::new("Diff with last commit"))
            .clicked()
        {
            self.diff_head(path, actions);
            ui.close();
        }
        let label = match connection {
            Some(c) => format!("Compare with {c}"),
            None => "Compare with database".into(),
        };
        if self.state.dialect.supports(Feature::ObjectFiles)
            && ui
                .add_enabled(sql && connection.is_some(), egui::Button::new(label))
                .on_disabled_hover_text("Connect first")
                .clicked()
            && let Some(c) = connection
        {
            self.compare_db(path, c, actions);
            ui.close();
        }
        if ui
            .add_enabled(sql, egui::Button::new("Deploy…"))
            .on_hover_text("Open a script running this file with @")
            .clicked()
        {
            let root = self.root.clone().unwrap_or_default();
            actions.push(ProjectAction::Deploy(deploy_script(
                &root,
                &[&path.to_owned()],
            )));
            ui.close();
        }
        ui.separator();
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(path.display().to_string());
            ui.close();
        }
    }

    /// A directory is marked when anything below it changed.
    fn dir_status(&self, dir: &Path) -> Option<FileStatus> {
        let repo = self.state.repo.as_ref()?;
        repo.status
            .keys()
            .any(|p| p.starts_with(dir))
            .then_some(FileStatus::Modified)
    }

    pub fn diff_head(&mut self, path: &Path, actions: &mut Vec<ProjectAction>) {
        let Some(repo) = &self.state.repo else { return };
        let committed = crate::git::head_text(&repo.root, path).unwrap_or_default();
        match crate::text_file::read(path).map(|f| f.text) {
            Ok(current) => actions.push(ProjectAction::Diff {
                title: format!("{} (changes)", file_name(path)),
                left: ("HEAD".into(), committed),
                right: ("Working copy".into(), current),
                file: path.to_owned(),
            }),
            Err(e) => self.state.message = Some((format!("{}: {e}", path.display()), true)),
        }
    }

    pub fn compare_db(&mut self, path: &Path, connection: &str, actions: &mut Vec<ProjectAction>) {
        let text = match crate::text_file::read(path).map(|f| f.text) {
            Ok(t) => t,
            Err(e) => {
                self.state.message = Some((format!("{}: {e}", path.display()), true));
                return;
            }
        };
        let Some((owner, name, object_type)) = object_of_file(path, &text) else {
            self.state.message = Some((
                format!(
                    "{}: no CREATE statement or known file type",
                    file_name(path)
                ),
                true,
            ));
            return;
        };
        let result = Arc::new(Mutex::new(None));
        let sink = result.clone();
        let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
            let outcome = (|| {
                let owner = match owner {
                    Some(o) => o,
                    None => conn.query_row_as::<String>(
                        "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
                        &[],
                    )?,
                };
                let db = object_text(conn, &owner, &name, object_type)?;
                let label = format!(
                    "{} {owner}.{name}",
                    object_type.dictionary_name().to_lowercase()
                );
                Ok::<_, pythia_db::DbError>((label, db))
            })();
            *sink.lock().expect("compare result") = Some(outcome.map_err(|e| e.to_string()));
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        });
        self.state.compare = Some(Compare {
            file: path.to_owned(),
            connection: connection.to_owned(),
            result,
        });
        self.state.message = None;
        actions.push(ProjectAction::Request(
            connection.to_owned(),
            DbRequest::Task {
                id: request_id(),
                task,
            },
        ));
    }

    /// Starts exporting the schema of `connection` into `dir`.
    pub fn export_schema(
        &mut self,
        dir: PathBuf,
        connection: &str,
        actions: &mut Vec<ProjectAction>,
    ) {
        let progress = Arc::new(ExportProgress::default());
        let result = Arc::new(Mutex::new(None));
        let (task_progress, sink, task_dir) = (progress.clone(), result.clone(), dir.clone());
        let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
            let outcome = conn
                .query_row_as::<String>(
                    "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
                    &[],
                )
                .map_err(pythia_db::DbError::from)
                .and_then(|owner| {
                    crate::object_files::export_schema(conn, &owner, &task_dir, &task_progress)
                });
            *sink.lock().expect("export result") = Some(outcome.map_err(|e| e.to_string()));
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        });
        self.state.export = Some(Export {
            dir,
            progress,
            result,
        });
        self.state.message = None;
        actions.push(ProjectAction::Request(
            connection.to_owned(),
            DbRequest::Task {
                id: request_id(),
                task,
            },
        ));
    }

    fn pick(&mut self, pick: Pick) {
        let (tx, rx) = std::sync::mpsc::channel();
        let start = self.root.clone();
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(dir) = start {
                dialog = dialog.set_directory(dir);
            }
            let _ = tx.send(dialog.pick_folder());
        });
        self.state.pick = Some((rx, pick));
    }

    /// Folder dialogs and background work that finished.
    fn poll(&mut self, ui: &Ui, actions: &mut Vec<ProjectAction>) {
        if let Some((rx, pick)) = &self.state.pick {
            match rx.try_recv() {
                Ok(dir) => {
                    let pick = *pick;
                    self.state.pick = None;
                    match (pick, dir) {
                        (Pick::Root, Some(dir)) => {
                            self.root = Some(dir);
                            self.expanded.clear();
                            self.state = State::default();
                        }
                        (Pick::ExportTo, Some(dir)) => self.state.pending_export = Some(dir),
                        (_, None) => {}
                    }
                }
                Err(_) => ui
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        let finished = self.state.export.as_ref().and_then(|e| {
            let result = e.result.lock().expect("export result").take();
            result.map(|r| (r, e.dir.clone()))
        });
        if let Some((result, dir)) = finished {
            self.state.export = None;
            self.state.message = Some(match result {
                Ok(n) => (format!("Exported {n} objects to {}", dir.display()), false),
                Err(e) => (format!("Export failed: {e}"), true),
            });
            if self.root.is_none() {
                self.root = Some(dir);
            }
            self.state.listings.clear();
            self.state.refreshed = None;
        }
        if let Some(compare) = &self.state.compare {
            let result = compare.result.lock().expect("compare result").take();
            match result {
                Some(Ok((label, db))) => {
                    let file = compare.file.clone();
                    let connection = compare.connection.clone();
                    self.state.compare = None;
                    match crate::text_file::read(&file).map(|f| f.text) {
                        Ok(current) => actions.push(ProjectAction::Diff {
                            title: format!("{} ↔ {connection}", file_name(&file)),
                            left: (format!("{label} ({connection})"), db),
                            right: (file_name(&file), current),
                            file,
                        }),
                        Err(e) => {
                            self.state.message = Some((format!("{}: {e}", file.display()), true))
                        }
                    }
                }
                Some(Err(e)) => {
                    self.state.compare = None;
                    self.state.message = Some((e, true));
                }
                None => ui
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
    }
}

fn list_dir(dir: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let dir = e.file_type().ok()?.is_dir();
            if name.starts_with('.') && name != ".github"
                || dir && HIDDEN_DIRS.contains(&name.as_str())
            {
                return None;
            }
            Some(Entry {
                path: e.path(),
                name,
                dir,
            })
        })
        .collect();
    // Folders first, then by name.
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    )
}

fn file_icon(path: &Path) -> Icon {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "pks" | "pck" => Icon::Package,
        "pkb" | "plb" => Icon::PackageBody,
        "prc" => Icon::Procedure,
        "fnc" => Icon::Function,
        "trg" => Icon::Trigger,
        "tps" => Icon::Type,
        "tpb" => Icon::TypeBody,
        "vw" => Icon::View,
        "sql" | "ddl" | "dml" => Icon::NewSql,
        _ => Icon::Text,
    }
}

fn status_color(ui: &Ui, status: FileStatus) -> Color32 {
    match status {
        FileStatus::Modified | FileStatus::Renamed => ui.visuals().warn_fg_color,
        FileStatus::Added | FileStatus::Untracked => Color32::from_rgb(0x2f, 0xa0, 0x4c),
        FileStatus::Deleted | FileStatus::Conflicted => ui.visuals().error_fg_color,
    }
}

/// A script running `files` with `@`, in a stable order: specs and types
/// before bodies so dependencies compile first.
pub fn deploy_script(root: &Path, files: &[&PathBuf]) -> String {
    let rank = |p: &Path| match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "sql" | "ddl" => 0,
        "tps" => 1,
        "pks" | "pck" => 2,
        "vw" => 3,
        "fnc" | "prc" => 4,
        "tpb" => 5,
        "pkb" | "plb" => 6,
        "trg" => 7,
        _ => 8,
    };
    let mut files: Vec<&PathBuf> = files.to_vec();
    files.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    let mut script = format!(
        "-- Deploy {} file(s) from {}\n-- Review, then run with F8.\nwhenever sqlerror exit\n",
        files.len(),
        root.display()
    );
    for f in files {
        let path = f.display().to_string();
        if path.contains(' ') {
            script.push_str(&format!("@\"{path}\"\n"));
        } else {
            script.push_str(&format!("@{path}\n"));
        }
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_deploy_scripts() {
        let files = [
            PathBuf::from("/p/packages/a.pkb"),
            PathBuf::from("/p/packages/a.pks"),
            PathBuf::from("/p/tables/t.sql"),
            PathBuf::from("/p/my dir/x.trg"),
        ];
        let refs: Vec<&PathBuf> = files.iter().collect();
        let script = deploy_script(Path::new("/p"), &refs);
        let lines: Vec<&str> = script.lines().filter(|l| l.starts_with('@')).collect();
        assert_eq!(
            lines,
            [
                "@/p/tables/t.sql",
                "@/p/packages/a.pks",
                "@/p/packages/a.pkb",
                "@\"/p/my dir/x.trg\""
            ]
        );
    }
}
