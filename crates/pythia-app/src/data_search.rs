//! Data search: finds a value in every column of every table of a schema.
//!
//! One query per table counts the matches per eligible column (character
//! columns for text, numeric columns for numbers, dates for dates). Runs on
//! the window's own session with progress; Stop cancels between tables and
//! interrupts the running query.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use egui::{RichText, Ui};
use pythia_db::oracle::Connection;
use pythia_db::{DbError, DbEvent, DbRequest, Dialect};
use serde::{Deserialize, Serialize};

use crate::browser::{written, written_qualified};
use crate::documents::{DocContext, Effects, OpenRequest, Target, quote_ident, request_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Match {
    #[default]
    Contains,
    Exact,
    Regex,
}

#[derive(Default, Serialize, Deserialize)]
pub struct DataSearch {
    pub value: String,
    pub mode: Match,
    pub match_case: bool,
    /// Schema to search (empty: the current schema).
    pub owner: String,
    /// LIKE pattern on table names (empty: all).
    pub tables: String,
    /// Find columns by name instead of values in the data.
    #[serde(default)]
    pub column_names: bool,
    #[serde(skip)]
    state: State,
}

#[derive(Default)]
struct State {
    run: Option<Run>,
    hits: Vec<Hit>,
    error: Option<String>,
    finished: Option<String>,
}

struct Run {
    id: u64,
    progress: Arc<Progress>,
}

#[derive(Default)]
struct Progress {
    done: AtomicUsize,
    total: AtomicUsize,
    stop: AtomicBool,
    current: Mutex<String>,
    hits: Mutex<Vec<Hit>>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub owner: String,
    pub table: String,
    pub column: String,
    pub count: u64,
    /// WHERE condition selecting the matching rows (value inlined).
    pub condition: String,
    /// Column type, for column name matches.
    pub data_type: Option<String>,
}

/// What kind of columns a value can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Text,
    Number,
    Date,
}

fn value_kinds(value: &str, mode: Match) -> Vec<ValueKind> {
    let mut kinds = vec![ValueKind::Text];
    if mode == Match::Exact {
        if value.trim().parse::<f64>().is_ok() {
            kinds.push(ValueKind::Number);
        }
        if chrono_like_date(value) {
            kinds.push(ValueKind::Date);
        }
    }
    kinds
}

/// `YYYY-MM-DD` dates.
fn chrono_like_date(v: &str) -> bool {
    let b = v.trim().as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// The condition for one column.
fn condition(column: &str, kind: ValueKind, value: &str, mode: Match, match_case: bool) -> String {
    let col = quote_ident(column);
    match (kind, mode) {
        (ValueKind::Number, _) => format!("{col} = {}", value.trim()),
        (ValueKind::Date, _) => format!("trunc({col}) = date {}", quote(value.trim())),
        (ValueKind::Text, Match::Exact) if match_case => format!("{col} = {}", quote(value)),
        (ValueKind::Text, Match::Exact) => format!("upper({col}) = upper({})", quote(value)),
        (ValueKind::Text, Match::Contains) if match_case => {
            format!("instr({col}, {}) > 0", quote(value))
        }
        (ValueKind::Text, Match::Contains) => {
            format!("instr(upper({col}), upper({})) > 0", quote(value))
        }
        (ValueKind::Text, Match::Regex) => {
            format!(
                "regexp_like({col}, {}, '{}')",
                quote(value),
                if match_case { "c" } else { "i" }
            )
        }
    }
}

impl DataSearch {
    pub fn title(&self) -> String {
        if self.value.is_empty() {
            "Data search".into()
        } else {
            format!("Search data: {}", self.value)
        }
    }

    #[cfg(test)]
    pub fn hits(&self) -> &[Hit] {
        &self.state.hits
    }

    pub fn is_busy(&self) -> bool {
        self.state.run.is_some()
    }

    pub fn start(&mut self, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        let value = self.value.clone();
        if value.trim().is_empty() || self.is_busy() {
            return effects;
        }
        let owner = Some(self.owner.trim().to_uppercase())
            .filter(|o| !o.is_empty())
            .or_else(|| {
                cx.catalog
                    .map(|c| pythia_lsp_core::Catalog::current_schema(c).to_owned())
            })
            .unwrap_or_default();
        // Uppercased by Oracle, as the names it is compared with (#305).
        let tables = self.tables.trim().replace('*', "%");
        let (mode, match_case) = (self.mode, self.match_case);
        let progress = Arc::new(Progress::default());
        let id = request_id();
        self.state = State {
            run: Some(Run {
                id,
                progress: progress.clone(),
            }),
            ..State::default()
        };
        let column_names = self.column_names;
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            if column_names {
                find_columns(conn, &owner, &tables, &value, mode, match_case, &progress)?;
            } else {
                search(conn, &owner, &tables, &value, mode, match_case, &progress)?;
            }
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        if let DbEvent::TaskDone { id, result } = event
            && let Some(run) = self.state.run.take_if(|r| r.id == id)
        {
            self.state.hits = run.progress.hits.lock().expect("hits").clone();
            let stopped = run.progress.stop.load(Ordering::Relaxed);
            match result {
                Ok(_) => {
                    let n = run.progress.done.load(Ordering::Relaxed);
                    self.state.finished = Some(if self.column_names {
                        "Searched column names".to_owned()
                    } else if stopped {
                        format!("Stopped after {n} tables")
                    } else {
                        format!("Searched {n} tables")
                    });
                }
                Err(e) if stopped => self.state.finished = Some(format!("Stopped ({e})")),
                Err(e) => self.state.error = Some(e.to_string()),
            }
        }
        Effects::default()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        let mut run = false;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.column_names, false, "Values");
            ui.selectable_value(&mut self.column_names, true, "Column names");
            ui.separator();
            let hint = if self.column_names {
                "Column name to find"
            } else {
                "Value to find"
            };
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.value)
                    .hint_text(hint)
                    .desired_width(320.0),
            );
            run |= edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.selectable_value(&mut self.mode, Match::Contains, "Contains");
            ui.selectable_value(&mut self.mode, Match::Exact, "Exact");
            ui.selectable_value(&mut self.mode, Match::Regex, "Regex");
            ui.checkbox(&mut self.match_case, "Match case");
        });
        ui.horizontal(|ui| {
            ui.label("Schema");
            ui.add(
                egui::TextEdit::singleline(&mut self.owner)
                    .hint_text("current")
                    .desired_width(140.0),
            );
            ui.label("Tables");
            ui.add(
                egui::TextEdit::singleline(&mut self.tables)
                    .hint_text("all, or e.g. ORDER*")
                    .desired_width(160.0),
            );
            if let Some(r) = &self.state.run {
                if ui.button("Stop").clicked() {
                    r.progress.stop.store(true, Ordering::Relaxed);
                    effects.cancel = true;
                }
            } else if ui
                .add_enabled(
                    cx.connected && !self.value.trim().is_empty(),
                    egui::Button::image_and_text(
                        pythia_ui::icons::Icon::Search.image(ui),
                        "Search",
                    ),
                )
                .clicked()
            {
                run = true;
            }
        });
        if run && cx.connected {
            effects.extend(self.start(cx));
        }
        ui.separator();

        if let Some(r) = &self.state.run {
            let (done, total) = (
                r.progress.done.load(Ordering::Relaxed),
                r.progress.total.load(Ordering::Relaxed),
            );
            let current = r.progress.current.lock().expect("current").clone();
            ui.add(
                egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                    .text(format!("{done} / {total} tables · {current}")),
            );
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(150));
            // Show hits as they come in.
            self.state.hits = r.progress.hits.lock().expect("hits").clone();
        } else if let Some(e) = &self.state.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        } else if let Some(f) = &self.state.finished {
            ui.weak(format!(
                "{f} · {} column(s) with matches",
                self.state.hits.len()
            ));
        } else {
            ui.weak("Counts matches per column in every table of the schema. Large tables are scanned fully.");
        }
        ui.add_space(4.0);
        let mut open = None;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                egui::Grid::new("data-search-hits")
                    .striped(true)
                    .num_columns(3)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        if !self.state.hits.is_empty() {
                            ui.strong("Table");
                            ui.strong("Column");
                            ui.strong(if self.column_names { "Type" } else { "Rows" });
                            ui.end_row();
                        }
                        for hit in &self.state.hits {
                            if ui
                                .link(
                                    RichText::new(format!("{}.{}", hit.owner, hit.table))
                                        .monospace(),
                                )
                                .clicked()
                            {
                                open = Some(hit.clone());
                            }
                            ui.label(RichText::new(&hit.column).monospace());
                            match &hit.data_type {
                                Some(t) => ui.label(RichText::new(t.to_lowercase()).monospace()),
                                None => ui.label(RichText::new(hit.count.to_string()).monospace()),
                            };
                            ui.end_row();
                        }
                    });
            });
        if let (Some(hit), Some(connection)) = (open, cx.connection.clone()) {
            effects.open.push(OpenRequest::Sql {
                connection,
                text: hit_query(&hit),
                run: true,
            });
        }
        effects
    }
}

/// The query a hit opens: the rows that match, or for a column name the
/// table with that column first. Plain names read in lowercase, quoted
/// ones keep their case: "R71_Data" isn't "r71_data" (#334).
fn hit_query(hit: &Hit) -> String {
    let table = written_qualified(Dialect::ORACLE, &hit.owner, &hit.table);
    if hit.data_type.is_some() {
        format!(
            "select {}, t.*\n  from {table} t",
            written(Dialect::ORACLE, &hit.column)
        )
    } else {
        format!("select * from {table}\n where {}", hit.condition)
    }
}

/// Columns whose name matches, in tables and views of `owner`.
fn find_columns(
    conn: &Connection,
    owner: &str,
    tables: &str,
    pattern: &str,
    mode: Match,
    match_case: bool,
    progress: &Progress,
) -> Result<(), DbError> {
    let (condition, value) = match mode {
        Match::Exact if match_case => ("c.column_name = :3", pattern.trim().to_owned()),
        Match::Exact => (
            "upper(c.column_name) = upper(:3)",
            pattern.trim().to_owned(),
        ),
        Match::Contains => (
            "upper(c.column_name) like :3 escape '\\'",
            pythia_db::like_pattern(pattern.trim()),
        ),
        Match::Regex if match_case => ("regexp_like(c.column_name, :3)", pattern.to_owned()),
        Match::Regex => ("regexp_like(c.column_name, :3, 'i')", pattern.to_owned()),
    };
    let sql = format!(
        "select c.table_name, c.column_name, c.data_type
           from all_tab_columns c
          where c.owner = :1 and upper(c.table_name) like upper(:2) and {condition}
          order by c.table_name, c.column_id"
    );
    let tables = if tables.is_empty() { "%" } else { tables };
    let mut hits = Vec::new();
    for row in conn.query_as::<(String, String, String)>(&sql, &[&owner, &tables, &value])? {
        let (table, column, data_type) = row?;
        hits.push(Hit {
            owner: owner.to_owned(),
            table,
            column,
            count: 0,
            condition: String::new(),
            data_type: Some(data_type),
        });
    }
    progress.done.store(1, Ordering::Relaxed);
    *progress.hits.lock().expect("hits") = hits;
    Ok(())
}

fn search(
    conn: &Connection,
    owner: &str,
    tables: &str,
    value: &str,
    mode: Match,
    match_case: bool,
    progress: &Progress,
) -> Result<(), DbError> {
    let kinds = value_kinds(value, mode);
    let rows = conn.query_as::<(String, String, String)>(
        "select c.table_name, c.column_name, c.data_type from all_tab_columns c \
           join all_tables t on t.owner = c.owner and t.table_name = c.table_name \
          where c.owner = :1 and (:2 is null or upper(c.table_name) like upper(:2)) \
            and t.temporary = 'N' and nvl(t.iot_type, '-') <> 'IOT_OVERFLOW' \
          order by c.table_name, c.column_id",
        &[&owner, &Some(tables).filter(|t| !t.is_empty())],
    )?;
    // (table, [(column, kind)])
    let mut plan: Vec<(String, Vec<(String, ValueKind)>)> = Vec::new();
    for row in rows {
        let (table, column, data_type) = row?;
        let kind = match data_type.as_str() {
            "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" | "CLOB" | "NCLOB" => Some(ValueKind::Text),
            "NUMBER" | "FLOAT" | "BINARY_FLOAT" | "BINARY_DOUBLE" => Some(ValueKind::Number),
            t if t == "DATE" || t.starts_with("TIMESTAMP") => Some(ValueKind::Date),
            _ => None,
        }
        .filter(|k| kinds.contains(k));
        let Some(kind) = kind else { continue };
        match plan.last_mut() {
            Some((t, cols)) if *t == table => cols.push((column, kind)),
            _ => plan.push((table, vec![(column, kind)])),
        }
    }
    progress.total.store(plan.len(), Ordering::Relaxed);
    for (table, columns) in plan {
        if progress.stop.load(Ordering::Relaxed) {
            break;
        }
        *progress.current.lock().expect("current") = table.clone();
        let conditions: Vec<String> = columns
            .iter()
            .map(|(c, k)| condition(c, *k, value, mode, match_case))
            .collect();
        let counts: Vec<String> = conditions
            .iter()
            .map(|c| format!("count(case when {c} then 1 end)"))
            .collect();
        let sql = format!(
            "select {} from {}.{} where {}",
            counts.join(", "),
            quote_ident(owner),
            quote_ident(&table),
            conditions.join(" or ")
        );
        match conn.query(&sql, &[]) {
            Ok(mut rows) => {
                if let Some(Ok(row)) = rows.next() {
                    let mut hits = progress.hits.lock().expect("hits");
                    for (i, (column, _)) in columns.iter().enumerate() {
                        let count: u64 = row.get(i).unwrap_or(0);
                        if count > 0 {
                            hits.push(Hit {
                                owner: owner.to_owned(),
                                table: table.clone(),
                                column: column.clone(),
                                count,
                                condition: conditions[i].clone(),
                                data_type: None,
                            });
                        }
                    }
                }
            }
            // A table we can't read (or a bad regex) shouldn't stop the search.
            Err(e) if progress.stop.load(Ordering::Relaxed) => return Err(e.into()),
            Err(e) => tracing::debug!("data search on {table}: {e}"),
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions() {
        assert_eq!(
            condition("NAME", ValueKind::Text, "o'k", Match::Contains, false),
            "instr(upper(NAME), upper('o''k')) > 0"
        );
        assert_eq!(
            condition("ID", ValueKind::Number, " 42 ", Match::Exact, false),
            "ID = 42"
        );
        assert_eq!(
            condition("D", ValueKind::Date, "2024-01-31", Match::Exact, false),
            "trunc(D) = date '2024-01-31'"
        );
        assert_eq!(
            value_kinds("42", Match::Exact),
            [ValueKind::Text, ValueKind::Number]
        );
        assert_eq!(value_kinds("42", Match::Contains), [ValueKind::Text]);
    }

    /// The tables filter and a column name without Match case find quoted
    /// names with lowercase letters by any case (#305).
    #[test]
    fn finds_names_in_any_case() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let _ = conn.execute("drop table \"R62_Data\" purge", &[]);
        conn.execute(
            "create table \"R62_Data\" (\"Mixed\" varchar2(10), plain varchar2(10))",
            &[],
        )
        .unwrap();
        conn.execute("insert into \"R62_Data\" values ('needle', 'needle')", &[])
            .unwrap();
        conn.commit().unwrap();
        let owner = user.to_uppercase();
        let hits = |progress: Progress| {
            let hits = progress.hits.lock().unwrap();
            hits.iter()
                .map(|h| format!("{}.{}={}", h.table, h.column, h.count))
                .collect::<Vec<_>>()
        };
        let columns = |tables: &str, pattern: &str, mode| {
            let progress = Progress::default();
            find_columns(&conn, &owner, tables, pattern, mode, false, &progress).unwrap();
            hits(progress)
        };
        assert_eq!(
            columns("r62_d%", "mix", Match::Contains),
            ["R62_Data.Mixed=0"]
        );
        assert_eq!(
            columns("R62_DATA", "mixed", Match::Exact),
            ["R62_Data.Mixed=0"]
        );
        assert_eq!(
            columns("r62_data", "plain", Match::Exact),
            ["R62_Data.PLAIN=0"]
        );
        let progress = Progress::default();
        search(
            &conn,
            &owner,
            "r62_d%",
            "needle",
            Match::Exact,
            true,
            &progress,
        )
        .unwrap();
        assert_eq!(hits(progress), ["R62_Data.Mixed=1", "R62_Data.PLAIN=1"]);
        conn.execute("drop table \"R62_Data\" purge", &[]).unwrap();
    }

    /// A hit opens the rows of its own table and column, also where their
    /// names are quoted: "R71_Data" isn't "r71_data" (#334).
    #[test]
    fn hits_open_their_own_table() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        for t in ["\"R71_Data\"", "\"r71_data\""] {
            let _ = conn.execute(&format!("drop table {t} purge"), &[]);
        }
        for sql in [
            "create table \"R71_Data\" (\"Mixed\" varchar2(10), plain varchar2(10))",
            "insert into \"R71_Data\" values ('needle', 'needle')",
            "create table \"r71_data\" (\"Mixed\" varchar2(10))",
            "insert into \"r71_data\" values ('needle')",
            "insert into \"r71_data\" values ('needle')",
        ] {
            conn.execute(sql, &[]).unwrap();
        }
        conn.commit().unwrap();
        let owner = user.to_uppercase();
        // Each hit, and what the query it opens reads: its rows, or the error.
        let opened = |progress: Progress| {
            let hits = progress.hits.lock().unwrap();
            hits.iter()
                .map(|h| {
                    let read = match conn.query(&hit_query(h), &[]) {
                        Ok(rows) => rows.count().to_string(),
                        Err(e) => e.to_string().chars().take(9).collect(),
                    };
                    format!("{}.{}={} reads {read}", h.table, h.column, h.count)
                })
                .collect::<Vec<_>>()
        };
        let progress = Progress::default();
        search(
            &conn,
            &owner,
            "r71_d%",
            "needle",
            Match::Contains,
            false,
            &progress,
        )
        .unwrap();
        assert_eq!(
            opened(progress),
            [
                "R71_Data.Mixed=1 reads 1",
                "R71_Data.PLAIN=1 reads 1",
                "r71_data.Mixed=2 reads 2"
            ]
        );
        let progress = Progress::default();
        find_columns(
            &conn,
            &owner,
            "r71_d%",
            "mixed",
            Match::Exact,
            false,
            &progress,
        )
        .unwrap();
        assert_eq!(
            opened(progress),
            ["R71_Data.Mixed=0 reads 1", "r71_data.Mixed=0 reads 2"]
        );
        for t in ["\"R71_Data\"", "\"r71_data\""] {
            conn.execute(&format!("drop table {t} purge"), &[]).unwrap();
        }
    }
}
