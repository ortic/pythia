//! Export tables: several tables at once, each with an optional WHERE
//! clause, one file per table (optionally with its DDL), streamed so table
//! size doesn't matter.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use egui::{RichText, Ui};
use pythia_data::{Format, Options, StreamWriter};
use pythia_db::oracle::Connection;
use pythia_db::{DbError, DbEvent, DbRequest, ObjectType};
use serde::{Deserialize, Serialize};

use crate::documents::{DocContext, Effects, Target, quote_ident, request_id};

#[derive(Serialize, Deserialize)]
pub struct ExportTables {
    /// Schema to export from (empty: the current schema).
    pub owner: String,
    pub folder: Option<PathBuf>,
    #[serde(with = "format_name")]
    pub format: Format,
    pub header: bool,
    pub delimiter: u8,
    pub null_text: String,
    pub include_ddl: bool,
    #[serde(skip)]
    state: State,
}

/// Formats are stored by extension.
mod format_name {
    use pythia_data::Format;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(f: &Format, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(f.extension())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Format, D::Error> {
        let ext = String::deserialize(d)?;
        Ok(Format::STREAMING
            .iter()
            .copied()
            .find(|f| f.extension() == ext)
            .unwrap_or(Format::Csv))
    }
}

impl Default for ExportTables {
    fn default() -> Self {
        Self {
            owner: String::new(),
            folder: None,
            format: Format::Csv,
            header: true,
            delimiter: b',',
            null_text: String::new(),
            include_ddl: false,
            state: State::default(),
        }
    }
}

#[derive(Default)]
struct State {
    tables: Option<Result<Vec<Choice>, String>>,
    tables_request: Option<u64>,
    filter: String,
    picking: Option<Receiver<Option<PathBuf>>>,
    run: Option<Run>,
    message: Option<(bool, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct Choice {
    pub name: String,
    pub rows: Option<i64>,
    pub selected: bool,
    pub filter: String,
}

struct Run {
    id: u64,
    progress: Arc<Progress>,
}

#[derive(Default)]
pub struct Progress {
    pub tables_done: AtomicUsize,
    pub tables: AtomicUsize,
    pub rows: AtomicU64,
    pub current: Mutex<String>,
    pub stop: AtomicBool,
}

/// One table to export.
#[derive(Debug, Clone)]
pub struct Job {
    pub table: String,
    pub filter: String,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub owner: String,
    pub folder: PathBuf,
    pub format: Format,
    pub options: Options,
    pub include_ddl: bool,
}

impl ExportTables {
    pub fn title(&self) -> String {
        "Export tables".into()
    }

    pub fn is_busy(&self) -> bool {
        self.state.run.is_some() || self.state.tables_request.is_some()
    }

    #[cfg(test)]
    pub fn set_tables(&mut self, tables: &[(&str, &str)]) {
        self.state.tables = Some(Ok(tables
            .iter()
            .map(|(name, filter)| Choice {
                name: (*name).to_owned(),
                rows: None,
                selected: true,
                filter: (*filter).to_owned(),
            })
            .collect()));
    }

    #[cfg(test)]
    pub fn message(&self) -> Option<&(bool, String)> {
        self.state.message.as_ref()
    }

    #[cfg(test)]
    pub fn start_now(&mut self) -> Effects {
        self.start()
    }

    fn load_tables(&mut self) -> Effects {
        let mut effects = Effects::default();
        let owner = self.owner.trim().to_uppercase();
        let id = request_id();
        self.state.tables_request = Some(id);
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            let owner = if owner.is_empty() {
                conn.query_row_as::<String>(
                    "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
                    &[],
                )?
            } else {
                owner
            };
            let mut tables = Vec::new();
            for row in conn.query_as::<(String, Option<i64>)>(
                "select table_name, num_rows from all_tables
                  where owner = :1 and nested = 'NO' and secondary = 'N'
                    and table_name not like 'BIN$%'
                  order by table_name",
                &[&owner],
            )? {
                let (name, rows) = row?;
                tables.push(Choice {
                    name,
                    rows,
                    selected: false,
                    filter: String::new(),
                });
            }
            Ok(Box::new(tables) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    fn start(&mut self) -> Effects {
        let mut effects = Effects::default();
        let (Some(folder), Some(Ok(tables))) = (&self.folder, &self.state.tables) else {
            return effects;
        };
        let jobs: Vec<Job> = tables
            .iter()
            .filter(|t| t.selected)
            .map(|t| Job {
                table: t.name.clone(),
                filter: t.filter.trim().to_owned(),
            })
            .collect();
        if jobs.is_empty() {
            return effects;
        }
        let settings = Settings {
            owner: self.owner.trim().to_uppercase(),
            folder: folder.clone(),
            format: self.format,
            options: Options {
                header: self.header,
                delimiter: Some(self.delimiter),
                null_text: self.null_text.clone(),
                ..Options::default()
            },
            include_ddl: self.include_ddl,
        };
        let progress = Arc::new(Progress::default());
        let id = request_id();
        self.state.run = Some(Run {
            id,
            progress: progress.clone(),
        });
        self.state.message = None;
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            let rows = export(conn, &settings, &jobs, &progress)?;
            Ok(Box::new(rows) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        let DbEvent::TaskDone { id, result } = event else {
            return Effects::default();
        };
        let s = &mut self.state;
        if s.tables_request == Some(id) {
            s.tables_request = None;
            s.tables = Some(match result.map(|a| a.downcast::<Vec<Choice>>()) {
                Ok(Ok(t)) => Ok(*t),
                Ok(Err(_)) => Err("unexpected result".into()),
                Err(e) => Err(e.to_string()),
            });
        } else if let Some(run) = s.run.take_if(|r| r.id == id) {
            let tables = run.progress.tables_done.load(Ordering::Relaxed);
            let stopped = run.progress.stop.load(Ordering::Relaxed);
            s.message = Some(match result.map(|a| a.downcast::<u64>()) {
                Ok(Ok(rows)) => (
                    false,
                    format!(
                        "Exported {rows} rows from {tables} table(s) to {}",
                        self.folder.as_deref().unwrap_or(Path::new("")).display()
                    ),
                ),
                Ok(Err(_)) => (true, "unexpected result".into()),
                Err(_) if stopped => (true, format!("Stopped after {tables} table(s)")),
                Err(e) => (true, e.to_string()),
            });
        }
        Effects::default()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        if !cx.connected {
            ui.centered_and_justified(|ui| ui.weak("Connect to export tables"));
            return effects;
        }
        if self.state.tables.is_none() && self.state.tables_request.is_none() {
            effects.extend(self.load_tables());
        }
        if let Some(rx) = &self.state.picking
            && let Ok(picked) = rx.try_recv()
        {
            self.state.picking = None;
            if picked.is_some() {
                self.folder = picked;
            }
        }

        let mut reload = false;
        ui.horizontal(|ui| {
            ui.label("Schema");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.owner)
                    .hint_text("current")
                    .desired_width(140.0),
            );
            reload |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            reload |= ui.button("Reload").clicked();
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.state.filter)
                    .hint_text("Filter tables…")
                    .desired_width(180.0),
            );
            if let Some(Ok(tables)) = &mut self.state.tables {
                let needle = self.state.filter.trim().to_uppercase();
                if ui.small_button("Select all").clicked() {
                    for t in tables.iter_mut().filter(|t| t.name.contains(&needle)) {
                        t.selected = true;
                    }
                }
                if ui.small_button("None").clicked() {
                    for t in tables.iter_mut() {
                        t.selected = false;
                    }
                }
            }
        });
        if reload {
            self.state.tables = None;
            effects.extend(self.load_tables());
        }

        egui::Panel::bottom(egui::Id::new(("export-options", cx.id)))
            .resizable(false)
            .show(ui, |ui| effects.extend(self.options_ui(ui)));
        egui::CentralPanel::default().show(ui, |ui| self.tables_ui(ui));
        effects
    }

    fn tables_ui(&mut self, ui: &mut Ui) {
        let needle = self.state.filter.trim().to_uppercase();
        match &mut self.state.tables {
            None => {
                ui.spinner();
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e.as_str());
            }
            Some(Ok(tables)) if tables.is_empty() => {
                ui.weak("No tables in this schema");
            }
            Some(Ok(tables)) => {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        egui::Grid::new("export-tables")
                            .num_columns(3)
                            .striped(true)
                            .spacing([16.0, 4.0])
                            .show(ui, |ui| {
                                ui.strong("Table");
                                ui.strong("Rows (statistics)");
                                ui.strong("WHERE");
                                ui.end_row();
                                for t in tables.iter_mut().filter(|t| t.name.contains(&needle)) {
                                    ui.checkbox(
                                        &mut t.selected,
                                        RichText::new(t.name.to_lowercase()).monospace(),
                                    );
                                    ui.weak(t.rows.map_or_else(|| "?".into(), |r| r.to_string()));
                                    ui.add_enabled(
                                        t.selected,
                                        egui::TextEdit::singleline(&mut t.filter)
                                            .hint_text("all rows, or e.g. created > sysdate - 30")
                                            .font(egui::TextStyle::Monospace)
                                            .desired_width(360.0),
                                    );
                                    ui.end_row();
                                }
                            });
                    });
            }
        }
    }

    fn options_ui(&mut self, ui: &mut Ui) -> Effects {
        let mut effects = Effects::default();
        ui.add_space(4.0);
        egui::Grid::new("export-settings")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Format");
                ui.horizontal(|ui| {
                    for f in Format::STREAMING {
                        ui.selectable_value(&mut self.format, *f, f.label());
                    }
                });
                ui.end_row();
                if matches!(self.format, Format::Csv | Format::Tsv) {
                    ui.label("Options");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.header, "Header row");
                        if self.format == Format::Csv {
                            ui.label("Separator");
                            for (d, label) in
                                [(b',', ","), (b';', ";"), (b'|', "|"), (b'\t', "Tab")]
                            {
                                ui.selectable_value(&mut self.delimiter, d, label);
                            }
                        }
                        ui.label("NULL as");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.null_text)
                                .hint_text("empty")
                                .desired_width(70.0),
                        );
                    });
                    ui.end_row();
                }
                ui.label("Folder");
                ui.horizontal(|ui| {
                    match &self.folder {
                        Some(f) => {
                            ui.label(RichText::new(f.display().to_string()).monospace());
                        }
                        None => {
                            ui.weak("choose where the files go");
                        }
                    }
                    if ui.button("Choose…").clicked() && self.state.picking.is_none() {
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(rfd::FileDialog::new().pick_folder());
                        });
                        self.state.picking = Some(rx);
                    }
                    ui.checkbox(&mut self.include_ddl, "Include DDL")
                        .on_hover_text("Writes <table>.ddl.sql next to each data file");
                });
                ui.end_row();
            });
        ui.add_space(4.0);
        let selected = match &self.state.tables {
            Some(Ok(t)) => t.iter().filter(|t| t.selected).count(),
            _ => 0,
        };
        ui.horizontal(|ui| {
            if let Some(run) = &self.state.run {
                let p = &run.progress;
                let (done, total) = (
                    p.tables_done.load(Ordering::Relaxed),
                    p.tables.load(Ordering::Relaxed),
                );
                let current = p.current.lock().expect("current").clone();
                ui.spinner();
                ui.add(
                    egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                        .desired_width(220.0)
                        .text(format!("{done} / {total} tables")),
                );
                ui.weak(format!(
                    "{} · {} rows",
                    current.to_lowercase(),
                    p.rows.load(Ordering::Relaxed)
                ));
                if ui.button("Stop").clicked() {
                    p.stop.store(true, Ordering::Relaxed);
                    effects.cancel = true;
                }
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(150));
            } else {
                if let Some((error, text)) = &self.state.message {
                    let color = if *error {
                        ui.visuals().error_fg_color
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.colored_label(color, text);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = match selected {
                        1 => "Export 1 table".to_owned(),
                        n => format!("Export {n} tables"),
                    };
                    if ui
                        .add_enabled(
                            selected > 0 && self.folder.is_some(),
                            egui::Button::image_and_text(
                                pythia_ui::icons::Icon::Export.image(ui),
                                label,
                            ),
                        )
                        .clicked()
                    {
                        effects.extend(self.start());
                    }
                });
            }
        });
        ui.add_space(4.0);
        effects
    }
}

/// How a table's rows are written: INSERTs go into `table`, quoted unless
/// it is a plain name (#242).
fn insert_options(settings: &Settings, table: &str) -> Options {
    Options {
        table: pythia_data::identifier(table),
        ..settings.options.clone()
    }
}

/// Exports the tables; returns the total number of rows written.
pub fn export(
    conn: &Connection,
    settings: &Settings,
    jobs: &[Job],
    progress: &Progress,
) -> Result<u64, DbError> {
    let owner = if settings.owner.is_empty() {
        conn.query_row_as::<String>(
            "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
            &[],
        )?
    } else {
        settings.owner.clone()
    };
    progress.tables.store(jobs.len(), Ordering::Relaxed);
    std::fs::create_dir_all(&settings.folder)
        .map_err(|e| DbError::new(format!("{}: {e}", settings.folder.display())))?;
    let mut total = 0;
    for job in jobs {
        if progress.stop.load(Ordering::Relaxed) {
            return Err(DbError::new("stopped"));
        }
        *progress.current.lock().expect("current") = job.table.clone();
        let base = job.table.to_lowercase();
        if settings.include_ddl {
            let ddl =
                crate::object_files::object_text(conn, &owner, &job.table, ObjectType::Table)?;
            let path = settings.folder.join(format!("{base}.ddl.sql"));
            std::fs::write(&path, ddl)
                .map_err(|e| DbError::new(format!("{}: {e}", path.display())))?;
        }
        let mut sql = format!(
            "select * from {}.{}",
            quote_ident(&owner),
            quote_ident(&job.table)
        );
        if !job.filter.is_empty() {
            sql.push_str(&format!(" where {}", job.filter));
        }
        let path = settings
            .folder
            .join(format!("{base}.{}", settings.format.extension()));
        let io = |e: &dyn std::fmt::Display| DbError::new(format!("{}: {e}", path.display()));
        let file = std::fs::File::create(&path).map_err(|e| io(&e))?;
        let mut out = Some(std::io::BufWriter::with_capacity(1 << 20, file));
        let mut writer: Option<StreamWriter<std::io::BufWriter<std::fs::File>>> = None;
        let options = insert_options(settings, &job.table);
        let result = pythia_db::stream_query(conn, &sql, 1000, |columns, rows| {
            if progress.stop.load(Ordering::Relaxed) {
                return Err(DbError::new("stopped"));
            }
            if let Some(out) = out.take() {
                writer = Some(
                    StreamWriter::new(settings.format, columns.to_vec(), options.clone(), out)
                        .map_err(|e| io(&e))?,
                );
            }
            if let Some(w) = writer.as_mut() {
                w.write(rows).map_err(|e| io(&e))?;
            }
            progress
                .rows
                .fetch_add(rows.len() as u64, Ordering::Relaxed);
            Ok(())
        });
        match result {
            Ok(rows) => {
                if let Some(w) = writer.take() {
                    w.finish().map_err(|e| io(&e))?;
                }
                total += rows;
            }
            Err(e) => {
                drop(writer);
                let _ = std::fs::remove_file(&path);
                return Err(DbError::new(format!("{}: {}", job.table, e.message)));
            }
        }
        progress.tables_done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INSERTs name the table as Oracle reads it: a name stored other than
    /// in plain uppercase in quotes (#242).
    #[test]
    fn inserts_name_the_table() {
        let settings = Settings {
            owner: String::new(),
            folder: PathBuf::new(),
            format: Format::Insert,
            options: Options::default(),
            include_ddl: false,
        };
        let target = |table: &str| insert_options(&settings, table).table;
        assert_eq!(target("DEMO_EMPLOYEES"), "demo_employees");
        assert_eq!(target("guidelines-division"), "\"guidelines-division\"");
        assert_eq!(target("Foo"), "\"Foo\"");
        assert_eq!(target("MY TABLE"), "\"MY TABLE\"");
    }

    #[test]
    fn exports_tables_with_filters() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let _ = conn.execute("drop table pythia_exp_a purge", &[]);
        conn.execute(
            "create table pythia_exp_a as select level as id, case when mod(level, 2) = 0 then 'x' end as tag
               from dual connect by level <= 10",
            &[],
        )
        .unwrap();
        let folder = std::env::temp_dir().join(format!("pythia-exp-{}", std::process::id()));
        let settings = Settings {
            owner: String::new(),
            folder: folder.clone(),
            format: Format::Csv,
            options: Options {
                delimiter: Some(b';'),
                null_text: "NULL".into(),
                ..Options::default()
            },
            include_ddl: true,
        };
        let jobs = [Job {
            table: "PYTHIA_EXP_A".into(),
            filter: "id <= 3".into(),
        }];
        let progress = Progress::default();
        let rows = export(&conn, &settings, &jobs, &progress).unwrap();
        assert_eq!(rows, 3);
        let csv = std::fs::read_to_string(folder.join("pythia_exp_a.csv")).unwrap();
        assert_eq!(csv, "ID;TAG\n1;NULL\n2;x\n3;NULL\n");
        let ddl = std::fs::read_to_string(folder.join("pythia_exp_a.ddl.sql")).unwrap();
        assert!(ddl.contains("CREATE TABLE \"PYTHIA_EXP_A\""), "{ddl}");

        // A bad filter fails that table and leaves no partial file.
        let bad = [Job {
            table: "PYTHIA_EXP_A".into(),
            filter: "no_such_column = 1".into(),
        }];
        std::fs::remove_file(folder.join("pythia_exp_a.csv")).unwrap();
        let err = export(&conn, &settings, &bad, &Progress::default()).unwrap_err();
        assert!(err.message.contains("ORA-00904"), "{err}");
        assert!(!folder.join("pythia_exp_a.csv").exists());

        conn.execute("drop table pythia_exp_a purge", &[]).unwrap();
        std::fs::remove_dir_all(folder).unwrap();
    }
}
