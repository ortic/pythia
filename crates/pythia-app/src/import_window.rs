//! Import data from CSV or Excel into a new or existing table.
//!
//! Drop a file on the window (or pick one), check the preview and inferred
//! types, choose the target, import. Rows are inserted with array DML on the
//! window's own session; the transaction is committed only if asked to.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;

use egui::{RichText, Ui};
use pythia_data::import::{self, ColumnInfo, CsvOptions, Sheet};
use pythia_db::oracle::Connection;
use pythia_db::{Column, DbError, DbEvent, DbRequest, Dialect, MetaQuery, MetaResult, TableColumn};
use pythia_ui::icons::{Icon, icon_text_button};
use pythia_ui::{GridState, ResultGrid};
use serde::{Deserialize, Serialize};

use crate::browser::written;
use crate::documents::{DocContext, Effects, Target, quote_ident, request_id};

#[derive(Default, Serialize, Deserialize)]
pub struct ImportWindow {
    #[serde(skip)]
    state: State,
}

#[derive(Default)]
struct State {
    path: Option<PathBuf>,
    picking: Option<Receiver<Option<PathBuf>>>,
    sheets: Vec<String>,
    sheet_name: String,
    delimiter: Option<u8>,
    header: Option<bool>,
    data: Option<Result<Loaded, String>>,
    grid: GridState,
    target: TargetTable,
    table: String,
    existing: Option<Result<Vec<TableColumn>, String>>,
    columns_request: Option<u64>,
    /// Target column per source column (existing table).
    mapping: Vec<Option<String>>,
    commit: bool,
    /// Per source column: how values are cleaned up and converted.
    transforms: Vec<Transform>,
    /// Insert the good rows and log rejected ones instead of stopping.
    skip_errors: bool,
    run: Option<Run>,
    message: Option<(bool, String)>,
}

/// How a source column's values are cleaned up and converted.
#[derive(Debug, Clone, Default, PartialEq)]
struct Transform {
    trim: bool,
    /// Oracle date format, for date columns.
    date_format: Option<String>,
    /// SQL expression with `:value` for the converted value, e.g.
    /// `upper(:value)`; a constant if it doesn't use `:value`.
    expression: String,
}

impl Transform {
    fn for_column(info: &ColumnInfo) -> Self {
        Self {
            trim: true,
            date_format: match &info.kind {
                import::Kind::Date { oracle_format } => Some((*oracle_format).to_owned()),
                _ => None,
            },
            expression: String::new(),
        }
    }

    /// The SQL for bind `:n`.
    fn bind_expr(&self, n: usize, info: Option<&ColumnInfo>) -> String {
        let base = match &self.date_format {
            Some(format) => format!("to_date(:{n}, '{}')", format.replace('\'', "''")),
            None => import::bind_expr(n, info),
        };
        let expression = self.expression.trim();
        if expression.is_empty() {
            base
        } else if expression.contains("--") {
            // A `--` comment at its end would take the next value with it.
            format!("{}\n", replace_value(expression, &base))
        } else {
            replace_value(expression, &base)
        }
    }

    fn value(&self, raw: Option<&str>, info: Option<&ColumnInfo>) -> Option<String> {
        let raw = if self.trim { raw.map(str::trim) } else { raw };
        import::bind_value(raw.filter(|v| !v.is_empty()), info)
    }
}

/// How a file's column, whose values look like `info`, is converted into
/// a column of type `target`: a text column takes the value as it is (#456).
/// A phone number `+41 44 123 45 67` was stored as `41441234567`, `1.10` as
/// `1.1` and `01.02.2024` as a date's text, in VARCHAR2 columns.
fn conversion<'a>(
    info: Option<&'a import::ColumnInfo>,
    target: Option<&str>,
) -> Option<&'a import::ColumnInfo> {
    const TEXT: &[&str] = &[
        "CHAR", "NCHAR", "VARCHAR", "NVARCHAR", "CLOB", "NCLOB", "LONG",
    ];
    let text = target.is_some_and(|t| {
        let t = t.trim_start().to_ascii_uppercase();
        TEXT.iter().any(|k| t.starts_with(k)) && !t.starts_with("LONG RAW")
    });
    if text { None } else { info }
}

/// Replaces `:value` (any case, as a whole word) in an expression.
fn replace_value(expression: &str, with: &str) -> String {
    let lower = expression.to_ascii_lowercase();
    let mut out = String::new();
    let mut pos = 0;
    while let Some(found) = lower[pos..].find(":value") {
        let start = pos + found;
        let end = start + ":value".len();
        let word_end = lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        out.push_str(&expression[pos..start]);
        out.push_str(if word_end {
            with
        } else {
            &expression[start..end]
        });
        pos = end;
    }
    out.push_str(&expression[pos..]);
    out
}

/// What an import did.
struct Outcome {
    inserted: usize,
    rejected: usize,
    /// Rejected rows with their errors, if any were skipped.
    bad_file: Option<PathBuf>,
}

struct Loaded {
    sheet: Sheet,
    info: Vec<ColumnInfo>,
    /// Preview columns (header + inferred type).
    columns: Vec<Column>,
}

struct Run {
    id: u64,
    progress: Arc<AtomicUsize>,
    total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TargetTable {
    #[default]
    New,
    Existing,
}

impl ImportWindow {
    pub fn title(&self) -> String {
        match &self.state.path {
            Some(p) => format!(
                "Import {}",
                p.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default()
            ),
            None => "Import data".into(),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.state.run.is_some()
    }

    pub fn load(&mut self, path: PathBuf) {
        let s = &mut self.state;
        s.sheets = if is_workbook(&path) {
            import::workbook_sheets(&path).unwrap_or_default()
        } else {
            Vec::new()
        };
        if !s.sheets.contains(&s.sheet_name) {
            s.sheet_name = s.sheets.first().cloned().unwrap_or_default();
        }
        if s.table.is_empty() {
            let stem = path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            s.table = import::column_name(&stem);
        }
        s.path = Some(path);
        self.reload();
    }

    fn reload(&mut self) {
        let s = &mut self.state;
        let Some(path) = &s.path else { return };
        let sheet = if is_workbook(path) {
            import::read_workbook(path, &s.sheet_name, s.header)
        } else {
            std::fs::read(path)
                .map_err(pythia_data::Error::from)
                .and_then(|bytes| {
                    import::read_csv(
                        &bytes,
                        CsvOptions {
                            delimiter: s.delimiter,
                            header: s.header,
                        },
                    )
                })
        };
        s.data = Some(sheet.map_err(|e| e.to_string()).map(|sheet| {
            let info = import::infer(&sheet);
            let columns = sheet
                .headers
                .iter()
                .zip(&info)
                .map(|(h, i)| Column {
                    name: h.clone(),
                    type_name: import::oracle_type(i),
                    numeric: matches!(i.kind, import::Kind::Number { .. }),
                })
                .collect();
            Loaded {
                sheet,
                info,
                columns,
            }
        }));
        s.grid = GridState::default();
        s.transforms = match &s.data {
            Some(Ok(data)) => data.info.iter().map(Transform::for_column).collect(),
            _ => Vec::new(),
        };
        self.auto_map();
    }

    /// Maps source columns to target columns with the same (normalized) name.
    fn auto_map(&mut self) {
        let s = &mut self.state;
        let Some(Ok(data)) = &s.data else { return };
        let targets: Vec<String> = match &s.existing {
            Some(Ok(cols)) => cols.iter().map(|c| c.name.clone()).collect(),
            _ => Vec::new(),
        };
        s.mapping = data
            .sheet
            .headers
            .iter()
            .map(|h| {
                let wanted = import::column_name(h);
                targets.iter().find(|t| **t == wanted).cloned()
            })
            .collect();
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        let s = &mut self.state;
        match event {
            DbEvent::Meta { id, result } if s.columns_request == Some(id) => {
                s.columns_request = None;
                s.existing = Some(match result {
                    Ok(MetaResult::Columns(cols)) if cols.is_empty() => {
                        Err(format!("Table {} not found", s.table))
                    }
                    Ok(MetaResult::Columns(cols)) => Ok(cols),
                    Ok(_) => Err("unexpected result".into()),
                    Err(e) => Err(e.to_string()),
                });
                self.auto_map();
            }
            DbEvent::TaskDone { id, result } if s.run.as_ref().is_some_and(|r| r.id == id) => {
                s.run = None;
                s.message = Some(match result.map(|any| any.downcast::<Outcome>()) {
                    Ok(Ok(o)) => {
                        let mut text = format!("Imported {} rows", o.inserted);
                        if o.rejected > 0 {
                            text.push_str(&format!(", skipped {} bad rows", o.rejected));
                            if let Some(file) = &o.bad_file {
                                text.push_str(&format!(" (see {})", file.display()));
                            }
                        }
                        if s.commit {
                            text.push_str(" and committed");
                        } else {
                            text.push_str(" — commit (F10) or roll back (Shift+F10)");
                        }
                        (o.rejected > 0, text)
                    }
                    Ok(Err(_)) => (true, "unexpected result".into()),
                    Err(e) => (true, e.to_string()),
                });
            }
            _ => {}
        }
        Effects::default()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        if let Some(rx) = &self.state.picking
            && let Ok(picked) = rx.try_recv()
        {
            self.state.picking = None;
            if let Some(path) = picked {
                self.load(path);
            }
        }

        self.source_ui(ui);
        ui.separator();
        if !matches!(self.state.data, Some(Ok(_))) {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.add(Icon::Open.sized(ui, pythia_ui::icons::IconSize::Large));
                ui.heading("Drop a CSV or Excel file here");
                ui.weak(
                    "Delimiter, header row, numbers and date formats are detected automatically.",
                );
                if let Some(Err(e)) = &self.state.data {
                    ui.add_space(8.0);
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
            });
            return effects;
        }

        egui::Panel::bottom(egui::Id::new(("import-target", cx.id)))
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| effects.extend(self.target_ui(ui, cx)));
        egui::CentralPanel::default().show(ui, |ui| {
            let s = &mut self.state;
            if let Some(Ok(data)) = &s.data {
                ui.weak(format!(
                    "{} rows · {} columns · types inferred from the data",
                    data.sheet.rows.len(),
                    data.columns.len()
                ));
                ResultGrid::new(
                    &data.columns,
                    &data.sheet.rows,
                    &mut s.grid,
                    ("import-preview", cx.id),
                )
                .show(ui);
            }
        });
        effects
    }

    fn source_ui(&mut self, ui: &mut Ui) {
        let mut reload = false;
        ui.horizontal(|ui| {
            if icon_text_button(ui, Icon::Open, "Choose file…").clicked()
                && self.state.picking.is_none()
            {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let picked = rfd::FileDialog::new()
                        .add_filter(
                            "Data files",
                            &["csv", "tsv", "txt", "xlsx", "xls", "xlsb", "ods"],
                        )
                        .pick_file();
                    let _ = tx.send(picked);
                });
                self.state.picking = Some(rx);
            }
            let s = &mut self.state;
            match &s.path {
                Some(p) => {
                    ui.label(RichText::new(p.display().to_string()).monospace());
                }
                None => {
                    ui.weak("or drop a file on this window");
                }
            }
            if s.path.is_none() {
                return;
            }
            ui.separator();
            if !s.sheets.is_empty() {
                egui::ComboBox::from_id_salt("import-sheet")
                    .selected_text(&s.sheet_name)
                    .show_ui(ui, |ui| {
                        for name in s.sheets.clone() {
                            reload |= ui
                                .selectable_value(&mut s.sheet_name, name.clone(), name)
                                .changed();
                        }
                    });
            } else {
                let label = match s.delimiter {
                    None => "Auto",
                    Some(b',') => "Comma",
                    Some(b';') => "Semicolon",
                    Some(b'\t') => "Tab",
                    Some(_) => "Pipe",
                };
                ui.label("Delimiter");
                egui::ComboBox::from_id_salt("import-delimiter")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (value, text) in [
                            (None, "Auto"),
                            (Some(b','), "Comma"),
                            (Some(b';'), "Semicolon"),
                            (Some(b'\t'), "Tab"),
                            (Some(b'|'), "Pipe"),
                        ] {
                            reload |= ui.selectable_value(&mut s.delimiter, value, text).changed();
                        }
                    });
            }
            let header = match s.header {
                None => "Auto",
                Some(true) => "Yes",
                Some(false) => "No",
            };
            ui.label("Header row");
            egui::ComboBox::from_id_salt("import-header")
                .selected_text(header)
                .show_ui(ui, |ui| {
                    for (value, text) in [(None, "Auto"), (Some(true), "Yes"), (Some(false), "No")]
                    {
                        reload |= ui.selectable_value(&mut s.header, value, text).changed();
                    }
                });
        });
        if reload {
            self.reload();
        }
    }

    fn target_ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        let s = &mut self.state;
        let Some(Ok(data)) = &s.data else {
            return effects;
        };
        ui.horizontal(|ui| {
            ui.strong("Import into");
            ui.selectable_value(&mut s.target, TargetTable::New, "New table");
            ui.selectable_value(&mut s.target, TargetTable::Existing, "Existing table");
            ui.add(
                egui::TextEdit::singleline(&mut s.table)
                    .hint_text("table or owner.table")
                    .desired_width(220.0),
            );
            if s.target == TargetTable::Existing
                && ui
                    .add_enabled(
                        cx.connected && !s.table.trim().is_empty(),
                        egui::Button::new("Load columns"),
                    )
                    .clicked()
            {
                let (owner, table) = split_name(
                    &s.table,
                    cx.catalog.map(pythia_lsp_core::Catalog::current_schema),
                );
                let id = request_id();
                s.columns_request = Some(id);
                s.existing = None;
                effects.request(
                    Target::Main,
                    DbRequest::Meta {
                        id,
                        query: MetaQuery::Columns {
                            owner,
                            table,
                            keys: false,
                        },
                    },
                );
            }
        });
        ui.add_space(4.0);
        let mut ready = !s.table.trim().is_empty();
        let mut start = false;
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 40.0)
            .auto_shrink([false, true])
            .show(ui, |ui| match s.target {
                TargetTable::New => {
                    ui.weak("The table is created with these columns:");
                    ui.label(RichText::new(create_table_sql(&s.table, data)).monospace());
                }
                TargetTable::Existing => match &s.existing {
                    None if s.columns_request.is_some() => {
                        ui.spinner();
                    }
                    None => {
                        ready = false;
                        ui.weak("Load the target table's columns to map the file's columns.");
                    }
                    Some(Err(e)) => {
                        ready = false;
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    Some(Ok(targets)) => {
                        egui::Grid::new("import-mapping")
                            .num_columns(3)
                            .spacing([12.0, 4.0])
                            .show(ui, |ui| {
                                for (i, header) in data.sheet.headers.iter().enumerate() {
                                    ui.label(RichText::new(header).strong());
                                    ui.weak(&data.columns[i].type_name);
                                    let selected = s.mapping.get(i).cloned().flatten();
                                    egui::ComboBox::from_id_salt(("map", i))
                                        .selected_text(selected.as_deref().unwrap_or("(skip)"))
                                        .show_ui(ui, |ui| {
                                            if ui
                                                .selectable_label(selected.is_none(), "(skip)")
                                                .clicked()
                                            {
                                                s.mapping[i] = None;
                                            }
                                            for t in targets {
                                                let label = format!(
                                                    "{}  {}",
                                                    t.name,
                                                    t.data_type.to_lowercase()
                                                );
                                                if ui
                                                    .selectable_label(
                                                        selected.as_deref() == Some(&t.name),
                                                        label,
                                                    )
                                                    .clicked()
                                                {
                                                    s.mapping[i] = Some(t.name.clone());
                                                }
                                            }
                                        });
                                    ui.end_row();
                                }
                            });
                        ready &= s.mapping.iter().any(Option::is_some);
                    }
                },
            });
        egui::CollapsingHeader::new("Column options and errors")
            .id_salt("import-options")
            .show(ui, |ui| {
                egui::Grid::new("import-transforms")
                    .num_columns(4)
                    .striped(true)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.strong("Column");
                        ui.strong("Trim");
                        ui.strong("Date format");
                        ui.strong("SQL expression");
                        ui.end_row();
                        for (i, header) in data.sheet.headers.iter().enumerate() {
                            let Some(t) = s.transforms.get_mut(i) else {
                                continue;
                            };
                            ui.label(RichText::new(header).monospace());
                            ui.checkbox(&mut t.trim, "");
                            match &mut t.date_format {
                                Some(format) => {
                                    ui.add(
                                        egui::TextEdit::singleline(format)
                                            .font(egui::TextStyle::Monospace)
                                            .desired_width(180.0),
                                    );
                                }
                                None => {
                                    ui.weak("—");
                                }
                            }
                            ui.add(
                                egui::TextEdit::singleline(&mut t.expression)
                                    .hint_text("e.g. upper(:value), nvl(:value, 'n/a'), sysdate")
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(300.0),
                            );
                            ui.end_row();
                        }
                    });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("On errors");
                    ui.selectable_value(&mut s.skip_errors, false, "Stop");
                    ui.selectable_value(&mut s.skip_errors, true, "Skip the row")
                        .on_hover_text("Bad rows are written to <file>.bad.csv with their errors");
                });
            });
        ui.horizontal(|ui| {
            ui.checkbox(&mut s.commit, "Commit when done");
            if let Some(run) = &s.run {
                let done = run.progress.load(Ordering::Relaxed);
                ui.add(
                    egui::ProgressBar::new(done as f32 / run.total.max(1) as f32)
                        .text(format!("{done} / {}", run.total))
                        .desired_width(240.0),
                );
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            } else if let Some((error, text)) = &s.message {
                let color = if *error {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().text_color()
                };
                ui.colored_label(color, text);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let enabled = ready && cx.connected && s.run.is_none();
                let label = format!("Import {} rows", data.sheet.rows.len());
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::image_and_text(Icon::Execute.image(ui), label),
                    )
                    .clicked()
                {
                    start = true;
                }
            });
        });
        if start {
            effects.extend(self.start());
        }
        effects
    }

    /// Sets up a new-table import (tests).
    #[cfg(test)]
    pub fn test_new_table(&mut self, table: &str, commit: bool) -> Effects {
        self.state.target = TargetTable::New;
        self.state.table = table.to_owned();
        self.state.commit = commit;
        self.start()
    }

    #[cfg(test)]
    pub fn test_message(&self) -> Option<(bool, String)> {
        self.state.message.clone()
    }

    fn start(&mut self) -> Effects {
        let mut effects = Effects::default();
        let s = &mut self.state;
        let Some(Ok(data)) = &s.data else {
            return effects;
        };
        let table = s.table.trim().to_owned();
        // (source column index, target column name) pairs.
        let pairs: Vec<(usize, String)> = match s.target {
            TargetTable::New => (0..data.sheet.headers.len())
                .map(|i| (i, import::column_name(&data.sheet.headers[i])))
                .collect(),
            TargetTable::Existing => s
                .mapping
                .iter()
                .enumerate()
                .filter_map(|(i, m)| Some((i, m.clone()?)))
                .collect(),
        };
        let create = (s.target == TargetTable::New).then(|| create_table_sql(&table, data));
        let names: Vec<String> = pairs.iter().map(|(_, n)| quote_ident(n)).collect();
        let default = Transform::default();
        let transform = |i: usize| s.transforms.get(i).unwrap_or(&default);
        // Each source column's conversion, for the type of its target
        // column in an existing table.
        let target_type = |name: &str| match (&s.target, &s.existing) {
            (TargetTable::Existing, Some(Ok(columns))) => columns
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.data_type.as_str()),
            _ => None,
        };
        let infos: Vec<Option<&import::ColumnInfo>> = pairs
            .iter()
            .map(|(i, name)| conversion(data.info.get(*i), target_type(name)))
            .collect();
        let binds: Vec<String> = pairs
            .iter()
            .enumerate()
            .map(|(n, (i, _))| transform(*i).bind_expr(n + 1, infos[n]))
            .collect();
        let insert = format!(
            "insert into {} ({}) values ({})",
            table_ident(&table),
            names.join(", "),
            binds.join(", ")
        );
        let rows: Vec<Vec<Option<String>>> = data
            .sheet
            .rows
            .iter()
            .map(|row| {
                pairs
                    .iter()
                    .enumerate()
                    .map(|(n, (i, _))| {
                        transform(*i).value(row.get(*i).and_then(|v| v.as_deref()), infos[n])
                    })
                    .collect()
            })
            .collect();
        let progress = Arc::new(AtomicUsize::new(0));
        let commit = s.commit;
        // Rejected rows go next to the source file.
        let bad_file = s.skip_errors.then(|| {
            let path = s.path.clone().unwrap_or_else(|| PathBuf::from("import"));
            let stem = path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "import".into());
            path.with_file_name(format!("{stem}.bad.csv"))
        });
        let headers: Vec<String> = pairs.iter().map(|(_, n)| n.clone()).collect();
        let id = request_id();
        s.run = Some(Run {
            id,
            progress: progress.clone(),
            total: rows.len(),
        });
        s.message = None;
        let task = Box::new(move |conn: &Connection| {
            let outcome = run_import(
                conn,
                create.as_deref(),
                &insert,
                &rows,
                &progress,
                commit,
                bad_file.as_deref().map(|f| (f, headers.as_slice())),
            )?;
            Ok(Box::new(outcome) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }
}

const BATCH: usize = 1000;

/// Inserts `rows` with array DML. With `bad_file` (path, column names), rows
/// the database rejects are skipped and written there with their errors;
/// otherwise the first error stops the import.
fn run_import(
    conn: &Connection,
    create: Option<&str>,
    insert: &str,
    rows: &[Vec<Option<String>>],
    progress: &AtomicUsize,
    commit: bool,
    bad_file: Option<(&std::path::Path, &[String])>,
) -> Result<Outcome, DbError> {
    // Numbers are bound as dot-decimal text.
    conn.execute("alter session set nls_numeric_characters = '.,'", &[])?;
    if let Some(ddl) = create {
        conn.execute(ddl, &[])?;
    }
    let mut builder = conn.batch(insert, BATCH);
    if bad_file.is_some() {
        builder.with_batch_errors();
    }
    let mut batch = builder.build()?;
    let mut rejected: Vec<(usize, String)> = Vec::new();
    for (start, chunk) in rows.chunks(BATCH).enumerate().map(|(n, c)| (n * BATCH, c)) {
        for (offset, row) in chunk.iter().enumerate() {
            let params: Vec<&dyn pythia_db::oracle::sql_type::ToSql> = row
                .iter()
                .map(|v| v as &dyn pythia_db::oracle::sql_type::ToSql)
                .collect();
            batch
                .append_row(&params)
                .map_err(|e| row_error(e, start + offset))?;
        }
        match batch.execute() {
            Ok(()) => {}
            Err(e) if bad_file.is_some() && e.batch_errors().is_some() => {
                rejected.extend(e.batch_errors().into_iter().flatten().map(|e| {
                    // The first line says what's wrong; the rest is help text.
                    let first = e.message().lines().next().unwrap_or_default();
                    (start + e.offset() as usize, first.trim().to_owned())
                }));
            }
            Err(e) => return Err(row_error(e, start + chunk.len())),
        }
        progress.store(start + chunk.len(), Ordering::Relaxed);
    }
    let written = match bad_file {
        Some((path, headers)) if !rejected.is_empty() => {
            rejected.sort();
            write_bad_rows(path, headers, rows, &rejected)
                .map_err(|e| DbError::new(format!("{}: {e}", path.display())))?;
            Some(path.to_owned())
        }
        _ => None,
    };
    if commit {
        conn.commit()?;
    }
    Ok(Outcome {
        inserted: rows.len() - rejected.len(),
        rejected: rejected.len(),
        bad_file: written,
    })
}

/// Rejected rows as CSV: the values, the 1-based data row and the error.
fn write_bad_rows(
    path: &std::path::Path,
    headers: &[String],
    rows: &[Vec<Option<String>>],
    rejected: &[(usize, String)],
) -> Result<(), pythia_data::Error> {
    let mut writer = csv::Writer::from_path(path).map_err(pythia_data::Error::from)?;
    let mut header: Vec<&str> = headers.iter().map(String::as_str).collect();
    header.extend(["ROW", "ERROR"]);
    writer.write_record(&header)?;
    for (row, error) in rejected {
        let mut record: Vec<String> = rows[*row]
            .iter()
            .map(|v| v.clone().unwrap_or_default())
            .collect();
        record.push((row + 1).to_string());
        record.push(error.clone());
        writer.write_record(&record)?;
    }
    writer.flush()?;
    Ok(())
}

fn row_error(e: pythia_db::oracle::Error, row: usize) -> DbError {
    let mut error = DbError::from(e);
    error.message = format!("{} (in the batch up to row {row})", error.message);
    error
}

fn create_table_sql(table: &str, data: &Loaded) -> String {
    let columns: Vec<String> = data
        .sheet
        .headers
        .iter()
        .zip(&data.info)
        .map(|(h, info)| {
            let null = if info.nullable { "" } else { " not null" };
            format!(
                "  {} {}{null}",
                written(Dialect::ORACLE, &import::column_name(h)),
                import::oracle_type(info).to_lowercase()
            )
        })
        .collect();
    // Plain names in lowercase, quoted ones as the INSERT names them: a
    // table "My Table" isn't "my table", a column "DATE" isn't "date"
    // (#335).
    format!(
        "create table {} (\n{}\n)",
        table_name(table, |n| written(Dialect::ORACLE, n)),
        columns.join(",\n")
    )
}

/// `owner.table` or `table`, uppercased unless quoted.
fn table_ident(name: &str) -> String {
    table_name(name, quote_ident)
}

/// `owner.table` or `table` with each part as Oracle reads it, written by
/// `write`.
fn table_name(name: &str, write: impl Fn(&str) -> String) -> String {
    Dialect::ORACLE
        .typed_parts(name)
        .iter()
        .map(|p| write(p))
        .collect::<Vec<_>>()
        .join(".")
}

/// The owner and table typed, as Oracle reads them: the parts in quotes as
/// written, dots and all, others in uppercase (#369).
fn split_name(name: &str, current_schema: Option<&str>) -> (String, String) {
    let mut parts = Dialect::ORACLE.typed_parts(name);
    let table = parts.pop().unwrap_or_default();
    let owner = parts
        .pop()
        .unwrap_or_else(|| current_schema.unwrap_or_default().to_owned());
    (owner, table)
}

fn is_workbook(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("xlsx" | "xls" | "xlsb" | "xlsm" | "ods")
    )
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    /// A table typed as Oracle reads it: quoted parts as written, with
    /// their dots, others in uppercase (#369).
    #[test]
    fn tables_read_as_typed() {
        let split = |name| split_name(name, Some("HR"));
        assert_eq!(split("\"R83.Dot\""), ("HR".into(), "R83.Dot".into()));
        assert_eq!(split("app.\"My.T\""), ("APP".into(), "My.T".into()));
        assert_eq!(split(" emp "), ("HR".into(), "EMP".into()));
        assert_eq!(split("\"a\"\"b\""), ("HR".into(), "a\"b".into()));
        assert_eq!(table_ident("app.\"My.T\""), "APP.\"My.T\"");
        assert_eq!(table_ident("\"R83.Dot\""), "\"R83.Dot\"");
        assert_eq!(table_ident("emp"), "EMP");
    }

    /// A text column of an existing table takes the file's values as they
    /// are, whatever they look like (#456); a number or date column has them
    /// converted.
    #[test]
    fn text_columns_take_values_as_they_are() {
        let sheet = import::Sheet {
            headers: vec!["PHONE".into(), "DAY".into()],
            rows: vec![
                vec![Some("+41 44 123 45 67".into()), Some("01.02.2024".into())],
                vec![Some("1.10".into()), Some("31.12.2024".into())],
            ],
        };
        let infos = import::infer(&sheet);
        let phone = Some(&infos[0]);
        assert!(matches!(infos[0].kind, import::Kind::Number { .. }));
        let day = Some(&infos[1]);
        let t = Transform::default();
        for target in ["VARCHAR2(20 CHAR)", "nvarchar2(20)", "CLOB", "CHAR(16)"] {
            let info = conversion(phone, Some(target));
            assert_eq!(t.bind_expr(1, info), ":1", "{target}");
            assert_eq!(
                t.value(Some("+41 44 123 45 67"), info).as_deref(),
                Some("+41 44 123 45 67")
            );
            assert_eq!(t.bind_expr(2, conversion(day, Some(target))), ":2");
        }
        assert_eq!(
            t.bind_expr(1, conversion(phone, Some("NUMBER(12)"))),
            "to_number(:1)"
        );
        assert!(
            t.bind_expr(2, conversion(day, Some("DATE")))
                .starts_with("to_date(:2")
        );
        // A new table's columns are the file's.
        assert_eq!(t.bind_expr(1, conversion(phone, None)), "to_number(:1)");
    }

    #[test]
    fn expressions_wrap_the_value() {
        assert_eq!(replace_value("upper(:value)", ":1"), "upper(:1)");
        assert_eq!(
            replace_value("nvl(:VALUE, :value2)", ":3"),
            "nvl(:3, :value2)"
        );
        assert_eq!(replace_value("sysdate", ":1"), "sysdate");
        let t = Transform {
            trim: true,
            date_format: Some("DD.MM.YYYY".into()),
            expression: "trunc(:value, 'MM')".into(),
        };
        assert_eq!(
            t.bind_expr(2, None),
            "trunc(to_date(:2, 'DD.MM.YYYY'), 'MM')"
        );
        assert_eq!(t.value(Some("  x "), None).as_deref(), Some("x"));
        // A `--` comment ends with the line, not with the next value.
        let commented = Transform {
            expression: "upper(:value) -- in capitals".into(),
            ..Transform::default()
        };
        assert_eq!(commented.bind_expr(1, None), "upper(:1) -- in capitals\n");
        assert_eq!(t.value(Some("   "), None), None);
    }

    #[test]
    fn skips_and_logs_bad_rows() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let _ = conn.execute("drop table pythia_import_bad purge", &[]);
        conn.execute(
            "create table pythia_import_bad (id number primary key, name varchar2(5))",
            &[],
        )
        .unwrap();
        let rows: Vec<Vec<Option<String>>> = [
            ("1", "ann"),
            ("2", "bob"),
            ("1", "dup"),
            ("3", "much too long"),
            ("4", "eve"),
        ]
        .iter()
        .map(|(id, name)| vec![Some((*id).into()), Some((*name).into())])
        .collect();
        let bad = std::env::temp_dir().join(format!("pythia-bad-{}.csv", std::process::id()));
        let headers = ["ID".to_owned(), "NAME".to_owned()];
        let outcome = run_import(
            &conn,
            None,
            "insert into pythia_import_bad (id, name) values (to_number(:1), upper(:2))",
            &rows,
            &AtomicUsize::new(0),
            false,
            Some((&bad, &headers)),
        )
        .unwrap();
        assert_eq!((outcome.inserted, outcome.rejected), (3, 2));
        let text = std::fs::read_to_string(&bad).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "ID,NAME,ROW,ERROR");
        assert!(lines[1].starts_with("1,dup,3,ORA-00001"), "{text}");
        assert!(
            lines[2].starts_with("3,much too long,4,\"ORA-12899"),
            "{text}"
        );
        let names: Vec<String> = conn
            .query_as::<String>("select name from pythia_import_bad order by id", &[])
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(names, ["ANN", "BOB", "EVE"]);
        conn.rollback().unwrap();
        conn.execute("drop table pythia_import_bad purge", &[])
            .unwrap();
        std::fs::remove_file(bad).unwrap();

        // Without skipping, the first bad row stops the import.
        conn.execute(
            "create table pythia_import_bad (id number primary key, name varchar2(5))",
            &[],
        )
        .unwrap();
        let err = run_import(
            &conn,
            None,
            "insert into pythia_import_bad (id, name) values (to_number(:1), :2)",
            &rows,
            &AtomicUsize::new(0),
            false,
            None,
        )
        .err()
        .unwrap();
        assert!(err.message.contains("ORA-00001"), "{err}");
        conn.rollback().unwrap();
        conn.execute("drop table pythia_import_bad purge", &[])
            .unwrap();
    }
}
