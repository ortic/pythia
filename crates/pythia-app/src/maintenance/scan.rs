//! The maintenance scan (#145): what a schema (Oracle, MySQL) or the
//! database (PostgreSQL, SQL Server) needs, found from the database's own
//! numbers. Each finding comes with its evidence and the statements that fix
//! it; the picked ones run as a batch, object by object, on the window's own
//! session (which autocommits), and the scan runs again afterwards.
//!
//! - Oracle: missing and stale statistics (locked ones too, which need
//!   force), unusable indexes and index (sub)partitions; for DBAs, whether
//!   the automatic statistics task is enabled and how its last runs went.
//! - PostgreSQL: dead rows, tables never vacuumed or analyzed, statistics
//!   behind, transaction ID age past `autovacuum_freeze_max_age`, invalid
//!   indexes.
//! - MySQL: reclaimable space, statistics older than the table's changes.
//! - SQL Server: fragmented indexes of some size, statistics with many
//!   changes, the last good `DBCC CHECKDB`.

use std::collections::{BTreeMap, BTreeSet};

use egui::{RichText, Ui};
use pythia_db::{Cell, Client, DbError, DbEvent, DbKind, DbRequest, Dialect, ObjectType};
use pythia_monitor::ui::style::{bytes, duration};
use pythia_ui::icons::Icon;
use serde::{Deserialize, Serialize};

use super::facts::{Facts, IndexSegment, Part, grouped};
use super::runner::{Planned, Runner};
use super::tasks::{self, Options, Task};
use super::{code, script};
use crate::documents::{DocContext, Effects, ObjectRef, OpenRequest, Target, request_id};

#[derive(Serialize, Deserialize)]
pub struct ScanWindow {
    /// The schema scanned (Oracle, MySQL); PostgreSQL and SQL Server scan
    /// the database the connection is in.
    pub schema: String,
    #[serde(skip)]
    state: State,
}

#[derive(Default)]
struct State {
    /// The scan was asked for (once shown).
    requested: bool,
    loading: Option<u64>,
    scan: Option<Scan>,
    error: Option<String>,
    /// The findings picked to run, by position.
    picked: BTreeSet<usize>,
    runner: Runner,
    /// Asking before a run on a production connection.
    confirm: bool,
    limits: Limits,
}

/// What a scan found.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// What was scanned: `schema APP`, `database shop`.
    pub scope: String,
    pub findings: Vec<Finding>,
    /// What else it learned, or couldn't check.
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub label: String,
    pub value: String,
    pub warn: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// The object, whose maintenance window shows the details; none for
    /// the database itself.
    pub object: Option<ObjectRef>,
    /// The object as shown: `app.orders`, `app.orders_ix`.
    pub name: String,
    pub problem: String,
    pub evidence: String,
    pub statements: Vec<String>,
    /// The table (or index) whose progress the database reports.
    pub target: Option<ObjectRef>,
    /// Keeps others from changing (or reading) the object while it runs.
    pub blocks: bool,
    /// Picked to run when the scan comes in: not where it takes force,
    /// locks the object, or is a heavy check.
    pub picked: bool,
}

/// Where the scan draws its lines; tests lower them.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// PostgreSQL: dead rows at least, and their share of the table in %.
    pub dead_rows: i64,
    pub dead_share: f64,
    /// PostgreSQL: rows changed since the last analyze, at least (and 10 %
    /// of the table, autovacuum's line).
    pub changed_rows: i64,
    /// MySQL: reclaimable bytes at least, and their share of the table.
    pub free_bytes: f64,
    pub free_share: f64,
    /// SQL Server: pages of a fragmented index at least.
    pub pages: i64,
    /// SQL Server: changes of a statistic at least (and 20 % of its rows).
    pub changes: i64,
    /// SQL Server: days since the last good `DBCC CHECKDB`.
    pub checkdb_days: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            dead_rows: 1000,
            dead_share: 20.0,
            changed_rows: 1000,
            // InnoDB keeps a few free extents anyway.
            free_bytes: 32.0 * 1024.0 * 1024.0,
            free_share: 0.2,
            // Microsoft's rule of thumb: leave small indexes alone,
            // reorganize over 5 %, rebuild over 30 %.
            pages: 1000,
            changes: 1000,
            checkdb_days: 7,
        }
    }
}

impl ScanWindow {
    pub fn new(schema: String) -> Self {
        Self {
            schema,
            state: State::default(),
        }
    }

    pub fn title(&self) -> String {
        if self.schema.is_empty() {
            "Maintenance scan".to_owned()
        } else {
            format!("{} maintenance scan", self.schema.to_lowercase())
        }
    }

    pub fn is_busy(&self) -> bool {
        self.state.loading.is_some() || self.state.runner.is_running()
    }

    /// Scans (again).
    pub fn load(&mut self) -> Effects {
        let mut effects = Effects::default();
        let id = request_id();
        self.state.requested = true;
        self.state.loading = Some(id);
        let schema = self.schema.clone();
        let limits = self.state.limits;
        let task: pythia_db::RunFn = Box::new(move |client| {
            let scan = scan(client, &schema, &limits)?;
            Ok(Box::new(scan) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Meta, DbRequest::Run { id, task });
        effects
    }

    pub fn on_event(&mut self, event: DbEvent, dialect: Dialect) -> Effects {
        let mut effects = Effects::default();
        match event {
            DbEvent::TaskDone { id, result } if self.state.loading == Some(id) => {
                self.state.loading = None;
                match result.map(|any| any.downcast::<Scan>()) {
                    Ok(Ok(scan)) => self.found(*scan),
                    Ok(Err(_)) => self.state.error = Some("Unexpected answer".into()),
                    Err(e) => self.state.error = Some(e.to_string()),
                }
            }
            event => {
                // What's fixed drops out of the next scan.
                if self.state.runner.on_event(event, dialect, &mut effects) {
                    effects.extend(self.load());
                }
            }
        }
        effects
    }

    fn found(&mut self, scan: Scan) {
        self.state.picked = scan
            .findings
            .iter()
            .enumerate()
            .filter(|(_, f)| f.picked)
            .map(|(i, _)| i)
            .collect();
        self.state.scan = Some(scan);
        self.state.error = None;
    }

    /// The picked findings.
    fn picked(&self) -> Vec<&Finding> {
        let Some(scan) = &self.state.scan else {
            return Vec::new();
        };
        self.state
            .picked
            .iter()
            .filter_map(|i| scan.findings.get(*i))
            .collect()
    }

    /// Runs the picked findings' statements, object by object.
    fn start(&mut self) -> Effects {
        let picked: Vec<Finding> = self.picked().into_iter().cloned().collect();
        let groups = picked
            .iter()
            .map(|f| format!("{} · {}", f.name, f.problem))
            .collect();
        let steps = picked
            .iter()
            .enumerate()
            .flat_map(|(group, f)| {
                f.statements.iter().map(move |sql| Planned {
                    group,
                    sql: sql.clone(),
                    target: f.target.clone(),
                })
            })
            .collect();
        self.state
            .runner
            .start("Maintenance".to_owned(), groups, steps)
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        let dialect = cx.dialect;
        if !cx.connected {
            ui.centered_and_justified(|ui| ui.weak("Connect to scan"));
            return effects;
        }
        if !self.state.requested {
            effects.extend(self.load());
        }
        let running = self.state.runner.is_running();
        let statements: Vec<String> = self
            .picked()
            .iter()
            .flat_map(|f| f.statements.iter().cloned())
            .collect();
        let ready = !statements.is_empty();

        ui.horizontal(|ui| {
            ui.add(Icon::Settings.sized(ui, pythia_ui::icons::IconSize::Small));
            let scope = self
                .state
                .scan
                .as_ref()
                .map_or_else(|| self.title(), |s| format!("Maintenance of {}", s.scope));
            ui.label(RichText::new(scope).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if running {
                    let stop = egui::Button::image_and_text(Icon::Stop.image(ui), "Break");
                    if ui
                        .add_enabled(!self.state.runner.cancelling(), stop)
                        .on_hover_text("Cancel the statement running; the rest don't run")
                        .clicked()
                    {
                        self.state.runner.cancel();
                        effects.cancel = true;
                    }
                } else {
                    let n = self.state.picked.len();
                    let run = egui::Button::image_and_text(
                        Icon::Execute.image(ui),
                        format!("Run {n} picked"),
                    );
                    let response = ui.add_enabled(ready && !cx.read_only, run);
                    let response = if cx.read_only {
                        response.on_disabled_hover_text("The connection is read-only")
                    } else {
                        response.on_hover_text(
                            "Run the picked fixes object by object on this window's own \
                             session, which commits each; a failure skips the rest of its \
                             object's",
                        )
                    };
                    if response.clicked() {
                        if cx.production {
                            self.state.confirm = true;
                        } else {
                            effects.extend(self.start());
                        }
                    }
                }
                if ui
                    .add_enabled(ready, egui::Button::new("Open in SQL window"))
                    .clicked()
                    && let Some(connection) = cx.connection.clone()
                {
                    effects.open.push(OpenRequest::Sql {
                        connection,
                        text: script(dialect, &statements),
                        run: false,
                    });
                }
                let busy = self.is_busy();
                let rescan = ui
                    .add_enabled_ui(!busy, |ui| {
                        pythia_ui::icons::icon_button(ui, Icon::Refresh, "Scan again")
                    })
                    .inner;
                if rescan.clicked() {
                    effects.extend(self.load());
                }
                if self.state.loading.is_some() {
                    ui.spinner();
                }
            });
        });
        ui.separator();

        if let Some(e) = &self.state.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        match self.state.scan.take() {
            None => {
                if self.state.error.is_none() {
                    ui.spinner();
                }
            }
            Some(scan) => {
                notes_ui(ui, &scan.notes);
                self.findings_ui(ui, cx, &scan, running, &mut effects);
                self.state.scan = Some(scan);
            }
        }
        if ready {
            ui.add_space(6.0);
            egui::CollapsingHeader::new(format!("Statements ({})", statements.len()))
                .id_salt(("maintenance-scan-statements", cx.id))
                .show(ui, |ui| code(ui, cx, &script(dialect, &statements)));
        }
        self.state.runner.ui(ui);
        effects.extend(self.state.runner.poll(dialect));
        self.confirm_ui(ui.ctx(), cx, statements.len(), &mut effects);
        effects
    }

    fn findings_ui(
        &mut self,
        ui: &mut Ui,
        cx: &DocContext,
        scan: &Scan,
        running: bool,
        effects: &mut Effects,
    ) {
        if scan.findings.is_empty() {
            ui.label(
                RichText::new(format!("✔  Nothing needs maintenance in {}", scan.scope))
                    .color(egui::Color32::from_rgb(0x2f, 0xa0, 0x4c)),
            );
            return;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Findings ({})", scan.findings.len())).strong());
            ui.add_enabled_ui(!running, |ui| {
                if ui.small_button("All").clicked() {
                    self.state.picked = (0..scan.findings.len()).collect();
                }
                if ui.small_button("None").clicked() {
                    self.state.picked.clear();
                }
            });
        });
        let warn = ui.visuals().warn_fg_color;
        egui::ScrollArea::vertical()
            .id_salt(("maintenance-findings", cx.id))
            .max_height((ui.available_height() * 0.5).max(160.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new(("maintenance-findings-grid", cx.id))
                    .striped(true)
                    .num_columns(5)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        for (i, f) in scan.findings.iter().enumerate() {
                            let mut on = self.state.picked.contains(&i);
                            if ui
                                .add_enabled(!running, egui::Checkbox::without_text(&mut on))
                                .changed()
                            {
                                if on {
                                    self.state.picked.insert(i);
                                } else {
                                    self.state.picked.remove(&i);
                                }
                            }
                            ui.label(RichText::new(&f.name).monospace());
                            ui.label(&f.problem);
                            let evidence = ui.weak(&f.evidence);
                            if let Some(first) = f.statements.first() {
                                evidence.on_hover_text(crate::object_editor::first_line(first));
                            }
                            ui.horizontal(|ui| {
                                if f.blocks {
                                    ui.label(RichText::new("locks").color(warn)).on_hover_text(
                                        "Keeps others from changing the object while it runs",
                                    );
                                }
                                if let (Some(object), Some(connection)) =
                                    (&f.object, cx.connection.clone())
                                    && ui
                                        .small_button("Maintenance…")
                                        .on_hover_text("The object's state and all its tasks")
                                        .clicked()
                                {
                                    effects.open.push(OpenRequest::Maintenance {
                                        connection,
                                        object: object.clone(),
                                        task: None,
                                    });
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
    }

    fn confirm_ui(
        &mut self,
        ctx: &egui::Context,
        cx: &DocContext,
        n: usize,
        effects: &mut Effects,
    ) {
        if !self.state.confirm {
            return;
        }
        let mut decision = None;
        let modal =
            egui::Modal::new(egui::Id::new(("maintenance-scan-confirm", cx.id))).show(ctx, |ui| {
                ui.set_max_width(400.0);
                ui.heading("Production connection");
                ui.add_space(6.0);
                ui.label(format!(
                    "Run {n} statement{} on {}?",
                    if n == 1 { "" } else { "s" },
                    cx.connection.as_deref().unwrap_or("the connection")
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Run").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        if let Some(yes) = decision {
            self.state.confirm = false;
            if yes {
                effects.extend(self.start());
            }
        }
    }

    /// Lower lines, for test tables of a few rows.
    #[cfg(test)]
    pub fn set_limits_for_test(&mut self, limits: Limits) {
        self.state.limits = limits;
    }

    #[cfg(test)]
    pub fn scan_for_test(&self) -> Option<&Scan> {
        self.state.scan.as_ref()
    }

    /// Picks the findings `pick` accepts, and only those.
    #[cfg(test)]
    pub fn pick_for_test(&mut self, pick: impl Fn(&Finding) -> bool) {
        let Some(scan) = &self.state.scan else {
            return;
        };
        self.state.picked = scan
            .findings
            .iter()
            .enumerate()
            .filter(|(_, f)| pick(f))
            .map(|(i, _)| i)
            .collect();
    }

    #[cfg(test)]
    pub fn run_for_test(&mut self) -> Effects {
        self.start()
    }

    /// How the last run ended, and each statement's group and status.
    #[cfg(test)]
    pub fn outcome_for_test(&self) -> (Option<(String, bool)>, Vec<super::runner::Ended>) {
        (
            self.state.runner.outcome().cloned(),
            self.state.runner.statuses(),
        )
    }
}

fn notes_ui(ui: &mut Ui, notes: &[Note]) {
    if notes.is_empty() {
        return;
    }
    egui::Grid::new("maintenance-scan-notes")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for note in notes {
                ui.weak(&note.label);
                if note.warn {
                    ui.label(RichText::new(&note.value).color(ui.visuals().warn_fg_color));
                } else {
                    ui.label(&note.value);
                }
                ui.end_row();
            }
        });
    ui.add_space(6.0);
}

// ----- the scans ------------------------------------------------------------

/// Scans `schema` (Oracle, MySQL, SQLite's attached database) or the
/// database.
pub fn scan(client: &mut dyn Client, schema: &str, limits: &Limits) -> Result<Scan, DbError> {
    match client.dialect().kind() {
        DbKind::Oracle => oracle(client, schema),
        DbKind::Postgres => postgres(client, limits),
        DbKind::MySql => mysql(client, schema, limits),
        DbKind::SqlServer => sqlserver(client, limits),
        DbKind::Sqlite => sqlite(client, schema, limits),
    }
}

/// A SQLite database (`main` or an attached one): free pages VACUUM would
/// give back, and tables with indexes the planner has no statistics of.
fn sqlite(c: &mut dyn Client, schema: &str, limits: &Limits) -> Result<Scan, DbError> {
    let d = Dialect::SQLITE;
    let schema = if schema.is_empty() { "main" } else { schema };
    let quoted = d.quote_ident(schema);
    let mut scan = Scan {
        scope: format!("database {schema}"),
        ..Scan::default()
    };
    let pragma = |c: &mut dyn Client, name: &str| -> Result<f64, DbError> {
        let rows = c.query(&format!("pragma {quoted}.{name}"), &[])?;
        Ok(rows.first().and_then(|r| num(r, 0)).unwrap_or(0.0))
    };
    let page = pragma(c, "page_size")?;
    let free = pragma(c, "freelist_count")? * page;
    let total = pragma(c, "page_count")? * page;
    scan.notes.push(Note {
        label: "File".to_owned(),
        value: format!("{} of which {} free", bytes(total), bytes(free)),
        warn: false,
    });
    if free >= limits.free_bytes && free > limits.free_share * total {
        let database = object(schema, schema, ObjectType::Table);
        scan.findings.push(Finding {
            object: None,
            name: format!("database {schema}"),
            problem: "Reclaimable space".to_owned(),
            evidence: format!(
                "{} of the file's {} are free pages",
                bytes(free),
                bytes(total)
            ),
            statements: statements(
                d,
                &database,
                Task::Vacuum,
                &Options::default(),
                &Facts::default(),
            ),
            target: None,
            // It rewrites the whole file, holding a write lock.
            blocks: true,
            picked: true,
        });
    }
    // Statistics only matter where there are indexes to choose from.
    let stats = c
        .query(
            &format!("select distinct tbl from {quoted}.sqlite_stat1"),
            &[],
        )
        .unwrap_or_default();
    let analyzed: BTreeSet<String> = stats.iter().map(|r| text(r, 0).to_lowercase()).collect();
    for row in c.query(
        &format!(
            "select t.name from {quoted}.sqlite_schema t \
              where t.type = 'table' and t.name not like 'sqlite!_%' escape '!' \
                and exists (select 1 from {quoted}.sqlite_schema i \
                             where i.type = 'index' and i.tbl_name = t.name) \
              order by t.name"
        ),
        &[],
    )? {
        let name = text(&row, 0);
        if analyzed.contains(&name.to_lowercase()) {
            continue;
        }
        let table = object(schema, &name, ObjectType::Table);
        scan.findings.push(Finding {
            name: shown(d, schema, &name),
            problem: "No statistics".to_owned(),
            evidence: "ANALYZE never ran on it: the planner guesses which index to use".to_owned(),
            statements: statements(
                d,
                &table,
                Task::Analyze,
                &Options::default(),
                &Facts::default(),
            ),
            target: None,
            object: Some(table),
            blocks: false,
            picked: true,
        });
    }
    Ok(scan)
}

fn text(row: &[Cell], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn int(row: &[Cell], i: usize) -> Option<i64> {
    let t = row.get(i)?.as_deref()?;
    t.parse::<i64>()
        .ok()
        .or_else(|| t.parse::<f64>().ok().map(|f| f as i64))
}

fn num(row: &[Cell], i: usize) -> Option<f64> {
    row.get(i)?.as_deref()?.parse().ok()
}

fn ago(secs: Option<f64>) -> String {
    match secs {
        Some(s) => format!("{} ago", duration(s)),
        None => "never".into(),
    }
}

fn note(scan: &mut Scan, label: &str, value: impl Into<String>, warn: bool) {
    scan.notes.push(Note {
        label: label.to_owned(),
        value: value.into(),
        warn,
    });
}

/// A task's statements; the scan only asks for what it can make.
fn statements(
    dialect: Dialect,
    object: &ObjectRef,
    task: Task,
    o: &Options,
    f: &Facts,
) -> Vec<String> {
    tasks::statements(dialect, object, task, o, f).unwrap_or_default()
}

fn object(owner: &str, name: &str, object_type: ObjectType) -> ObjectRef {
    ObjectRef {
        owner: owner.to_owned(),
        name: name.to_owned(),
        object_type,
    }
}

/// `owner.name` as shown: Oracle's in lowercase, as the browser lists them.
fn shown(dialect: Dialect, owner: &str, name: &str) -> String {
    if dialect == Dialect::ORACLE {
        format!("{owner}.{name}").to_lowercase()
    } else {
        format!("{owner}.{name}")
    }
}

// ----- Oracle ---------------------------------------------------------------

/// DBMS_STATS's name of the automatic statistics task.
const AUTO_STATS: &str = "auto optimizer stats collection";

fn oracle(c: &mut dyn Client, schema: &str) -> Result<Scan, DbError> {
    let d = Dialect::ORACLE;
    let mut scan = Scan {
        scope: format!("schema {}", schema.to_lowercase()),
        ..Scan::default()
    };
    // What changed since the last flush counts for staleness (needs
    // ANALYZE ANY; without it, staleness lags by a few minutes).
    let _ = c.execute("begin dbms_stats.flush_database_monitoring_info; end;");
    // Online rebuilds are an Enterprise Edition feature (Free and XE have
    // them too).
    let online = !c
        .query("select product from product_component_version", &[])
        .unwrap_or_default()
        .iter()
        .any(|r| text(r, 0).contains("Standard Edition"));

    // Unusable indexes and index (sub)partitions, by index: first, so
    // that statistics gathered after include them.
    let mut unusable: BTreeMap<String, (String, Vec<IndexSegment>)> = BTreeMap::new();
    for row in c.query(
        "select i.index_name, i.table_name, null, null
           from all_indexes i
          where i.owner = :1 and i.partitioned = 'NO' and i.status = 'UNUSABLE'
            and i.index_type not in ('LOB', 'IOT - TOP')
         union all
         select p.index_name, i.table_name, p.partition_name, null
           from all_ind_partitions p
           join all_indexes i on i.owner = p.index_owner and i.index_name = p.index_name
          where p.index_owner = :2 and p.status = 'UNUSABLE'
         union all
         select s.index_name, i.table_name, null, s.subpartition_name
           from all_ind_subpartitions s
           join all_indexes i on i.owner = s.index_owner and i.index_name = s.index_name
          where s.index_owner = :3 and s.status = 'UNUSABLE'
          order by 1, 3, 4",
        &[schema, schema, schema],
    )? {
        let (index, table) = (text(&row, 0), text(&row, 1));
        let part = match (row.get(2).cloned().flatten(), row.get(3).cloned().flatten()) {
            (Some(p), _) => Part::Partition(p),
            (_, Some(s)) => Part::Subpartition(s),
            _ => Part::Whole,
        };
        unusable
            .entry(index.clone())
            .or_insert_with(|| (table, Vec::new()))
            .1
            .push(IndexSegment {
                owner: schema.to_owned(),
                index,
                part,
                unusable: true,
            });
    }
    for (index, (table, segments)) in unusable {
        let parts = segments.iter().filter(|s| s.part != Part::Whole).count();
        let (problem, evidence) = if parts == 0 {
            (
                "Unusable index".to_owned(),
                format!("on {}: queries using it fail", table.to_lowercase()),
            )
        } else {
            (
                "Unusable partitions".to_owned(),
                format!(
                    "{parts} partition{} of the index on {}",
                    if parts == 1 { "" } else { "s" },
                    table.to_lowercase()
                ),
            )
        };
        scan.findings.push(Finding {
            object: Some(object(schema, &index, ObjectType::Index)),
            name: shown(d, schema, &index),
            problem,
            evidence,
            statements: segments
                .iter()
                .map(|s| tasks::rebuild(d, s, online))
                .collect(),
            target: Some(object(schema, &table, ObjectType::Table)),
            blocks: !online,
            picked: true,
        });
    }

    for row in c.query(
        "select s.table_name, s.num_rows, round((sysdate - s.last_analyzed) * 86400),
                s.stale_stats, s.stattype_locked
           from all_tab_statistics s
           join all_tables t on t.owner = s.owner and t.table_name = s.table_name
          where s.owner = :1 and s.object_type = 'TABLE'
            and t.temporary = 'N' and t.nested = 'NO' and t.secondary = 'N' and t.dropped = 'NO'
            and (t.iot_type is null or t.iot_type = 'IOT')
            and not exists (select 1 from all_external_tables e
                             where e.owner = t.owner and e.table_name = t.table_name)
            and (s.last_analyzed is null or s.stale_stats = 'YES')
          order by s.table_name",
        &[schema],
    )? {
        let name = text(&row, 0);
        let table = object(schema, &name, ObjectType::Table);
        let analyzed = num(&row, 2);
        let locked = row.get(4).cloned().flatten();
        let (problem, mut evidence) = match analyzed {
            None => ("No statistics".to_owned(), "never gathered".to_owned()),
            Some(secs) => (
                "Stale statistics".to_owned(),
                format!("over 10 % changed since {}", ago(Some(secs))),
            ),
        };
        if let Some(rows) = int(&row, 1) {
            evidence.push_str(&format!(", {} rows then", grouped(rows)));
        }
        let facts = Facts {
            stats_locked: locked.is_some(),
            ..Facts::default()
        };
        let options = Options {
            force: locked.is_some(),
            ..Options::default()
        };
        let (problem, picked) = match &locked {
            // Locked on purpose, as a rule: gathering would undo it.
            Some(kind) => {
                evidence.push_str(&format!(
                    "; locked ({}), so gathering takes force",
                    kind.to_lowercase()
                ));
                (format!("{problem} (locked)"), false)
            }
            None => (problem, true),
        };
        scan.findings.push(Finding {
            name: shown(d, schema, &name),
            problem,
            evidence,
            statements: statements(d, &table, Task::GatherStats, &options, &facts),
            target: Some(table.clone()),
            object: Some(table),
            blocks: false,
            picked,
        });
    }

    // The automatic statistics task, for who may read DBA_AUTOTASK_*.
    match c.query(
        "select status from dba_autotask_client where client_name = :1",
        &[AUTO_STATS],
    ) {
        Ok(rows) => {
            let status = rows.first().map(|r| text(r, 0)).unwrap_or_default();
            let enabled = status.eq_ignore_ascii_case("ENABLED");
            note(
                &mut scan,
                "Automatic statistics",
                if enabled {
                    "enabled".to_owned()
                } else {
                    format!(
                        "{}: stale statistics stay stale until gathered",
                        status.to_lowercase()
                    )
                },
                !enabled,
            );
            let runs = c
                .query(
                    "select job_status, to_char(job_start_time, 'YYYY-MM-DD HH24:MI'),
                            round((systimestamp - cast(job_start_time as timestamp with time zone))
                                  * 86400 / 86400)
                       from dba_autotask_job_history
                      where client_name = :1
                      order by job_start_time desc
                      fetch first 3 rows only",
                    &[AUTO_STATS],
                )
                .unwrap_or_default();
            match runs.first() {
                None if enabled => note(&mut scan, "Last runs", "none recorded", true),
                None => {}
                Some(last) => {
                    let failed = !text(last, 0).eq_ignore_ascii_case("SUCCEEDED");
                    let value = runs
                        .iter()
                        .map(|r| format!("{} {}", text(r, 1), text(r, 0).to_lowercase()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    note(&mut scan, "Last runs", value, failed);
                }
            }
        }
        Err(_) => note(
            &mut scan,
            "Automatic statistics",
            "can't be read (needs SELECT on DBA_AUTOTASK_CLIENT)",
            false,
        ),
    }
    Ok(scan)
}

// ----- PostgreSQL -----------------------------------------------------------

fn postgres(c: &mut dyn Client, limits: &Limits) -> Result<Scan, DbError> {
    let d = Dialect::POSTGRES;
    let version = c
        .query("show server_version_num", &[])?
        .first()
        .and_then(|r| int(r, 0))
        .unwrap_or(0);
    let facts = Facts {
        online: true,
        release: ((version / 10000) as i32, (version % 10000 / 100) as i32),
        ..Facts::default()
    };
    let mut scan = Scan::default();
    for row in c.query(
        "select n.nspname, c.relname, c.relkind, coalesce(s.n_live_tup, 0), \
                coalesce(s.n_dead_tup, 0), \
                extract(epoch from now() - greatest(s.last_vacuum, s.last_autovacuum)), \
                extract(epoch from now() - greatest(s.last_analyze, s.last_autoanalyze)), \
                coalesce(s.n_mod_since_analyze, 0), age(c.relfrozenxid), \
                current_setting('autovacuum_freeze_max_age'), current_database() \
           from pg_class c \
           join pg_namespace n on n.oid = c.relnamespace \
           left join pg_stat_all_tables s on s.relid = c.oid \
          where c.relkind in ('r', 'm') and n.nspname <> 'information_schema' \
            and n.nspname !~ '^pg_' \
          order by 1, 2",
        &[],
    )? {
        scan.scope = format!("database {}", text(&row, 10));
        let (schema, name) = (text(&row, 0), text(&row, 1));
        let object_type = if text(&row, 2) == "m" {
            ObjectType::MaterializedView
        } else {
            ObjectType::Table
        };
        let live = int(&row, 3).unwrap_or(0);
        let dead = int(&row, 4).unwrap_or(0);
        let vacuumed = num(&row, 5);
        let analyzed = num(&row, 6);
        let changed = int(&row, 7).unwrap_or(0);
        let age = int(&row, 8).unwrap_or(0);
        let max_age = int(&row, 9).unwrap_or(200_000_000);
        let share = 100.0 * dead as f64 / (live + dead).max(1) as f64;

        let mut problems: Vec<(&str, String)> = Vec::new();
        let wraparound = age > max_age;
        if wraparound {
            problems.push((
                "Transaction ID age",
                format!(
                    "{} (autovacuum_freeze_max_age {}): it must be frozen",
                    grouped(age),
                    grouped(max_age)
                ),
            ));
        }
        let dead_rows = dead >= limits.dead_rows && share > limits.dead_share;
        if dead_rows {
            problems.push((
                "Dead rows",
                format!(
                    "{} ({share:.0} %), vacuumed {}",
                    grouped(dead),
                    ago(vacuumed)
                ),
            ));
        }
        let unvacuumed = vacuumed.is_none() && live >= limits.dead_rows;
        if unvacuumed && !dead_rows {
            problems.push(("Never vacuumed", format!("{} rows", grouped(live))));
        }
        let unanalyzed = analyzed.is_none() && live > 0;
        if unanalyzed {
            problems.push(("Never analyzed", format!("{} rows", grouped(live))));
        }
        let behind = analyzed.is_some() && changed >= limits.changed_rows && changed > live / 10;
        if behind {
            problems.push((
                "Statistics behind",
                format!(
                    "{} rows changed since the last analyze {}",
                    grouped(changed),
                    ago(analyzed)
                ),
            ));
        }
        let Some((problem, _)) = problems.first() else {
            continue;
        };
        let table = object(&schema, &name, object_type);
        let (task, options) = if wraparound || dead_rows || unvacuumed {
            let options = Options {
                analyze: true,
                freeze: wraparound,
                ..Options::default()
            };
            (Task::Vacuum, options)
        } else {
            (Task::Analyze, Options::default())
        };
        scan.findings.push(Finding {
            name: shown(d, &schema, &name),
            problem: (*problem).to_owned(),
            evidence: problems
                .iter()
                .map(|(p, e)| {
                    if *p == *problem {
                        e.clone()
                    } else {
                        format!("{}: {e}", p.to_lowercase())
                    }
                })
                .collect::<Vec<_>>()
                .join(" · "),
            statements: statements(d, &table, task, &options, &facts),
            target: Some(table.clone()),
            object: Some(table),
            blocks: false,
            picked: true,
        });
    }
    if scan.scope.is_empty() {
        let db = c.query("select current_database()", &[])?;
        scan.scope = format!(
            "database {}",
            db.first().map(|r| text(r, 0)).unwrap_or_default()
        );
    }

    // Invalid indexes: a failed CREATE INDEX CONCURRENTLY leaves one
    // behind (one still being built is left alone).
    let concurrently = tasks::concurrently_possible(&facts);
    let building = if concurrently {
        " and not exists (select 1 from pg_stat_progress_create_index p \
                           where p.index_relid = i.indexrelid)"
    } else {
        ""
    };
    for row in c.query(
        &format!(
            "select n.nspname, ic.relname, tc.relname \
               from pg_index i \
               join pg_class ic on ic.oid = i.indexrelid \
               join pg_class tc on tc.oid = i.indrelid \
               join pg_namespace n on n.oid = ic.relnamespace \
              where not i.indisvalid and n.nspname !~ '^pg_'{building} \
              order by 1, 2"
        ),
        &[],
    )? {
        let (schema, index, table) = (text(&row, 0), text(&row, 1), text(&row, 2));
        let name = d.qualified(&schema, &index);
        let how = if concurrently { "concurrently " } else { "" };
        scan.findings.push(Finding {
            object: Some(object(&schema, &table, ObjectType::Table)),
            name: shown(d, &schema, &index),
            problem: "Invalid index".to_owned(),
            evidence: format!(
                "on {table}: left by a failed CREATE INDEX CONCURRENTLY; not used, but kept \
                 up to date"
            ),
            statements: vec![format!("reindex index {how}{name}")],
            target: Some(object(&schema, &index, ObjectType::Index)),
            blocks: !concurrently,
            picked: true,
        });
    }
    Ok(scan)
}

// ----- MySQL ----------------------------------------------------------------

fn mysql(c: &mut dyn Client, schema: &str, limits: &Limits) -> Result<Scan, DbError> {
    let d = Dialect::MYSQL;
    let mut scan = Scan {
        scope: format!("database {schema}"),
        ..Scan::default()
    };
    // MySQL 8 caches the tables' sizes for a day; MariaDB has no such
    // cache (and no variable).
    let _ = c.execute("set session information_schema_stats_expiry = 0");
    let free_bytes = limits.free_bytes.to_string();
    let free_share = limits.free_share.to_string();
    for row in c.query(
        "select table_name, engine, data_length, index_length, data_free \
           from information_schema.tables \
          where table_schema = ? and table_type = 'BASE TABLE' \
            and data_free >= ? and data_free > ? * (data_length + index_length) \
          order by data_free desc",
        &[schema, &free_bytes, &free_share],
    )? {
        let name = text(&row, 0);
        let engine = text(&row, 1);
        let size = num(&row, 2).unwrap_or(0.0) + num(&row, 3).unwrap_or(0.0);
        let free = num(&row, 4).unwrap_or(0.0);
        let table = object(schema, &name, ObjectType::Table);
        scan.findings.push(Finding {
            name: shown(d, schema, &name),
            problem: "Reclaimable space".to_owned(),
            evidence: format!(
                "{} free beside {} of data and indexes",
                bytes(free),
                bytes(size)
            ),
            statements: statements(
                d,
                &table,
                Task::Optimize,
                &Options::default(),
                &Facts::default(),
            ),
            target: None,
            object: Some(table),
            // InnoDB rebuilds online; MyISAM and Aria lock the table.
            blocks: !engine.eq_ignore_ascii_case("InnoDB"),
            picked: true,
        });
    }

    // InnoDB's persistent statistics, older than the table's last change
    // by a day or more.
    match c.query(
        "select s.table_name, timestampdiff(second, s.last_update, now()), \
                timestampdiff(second, t.update_time, now()) \
           from mysql.innodb_table_stats s \
           join information_schema.tables t \
             on t.table_schema = s.database_name and t.table_name = s.table_name \
          where s.database_name = ? and t.update_time > s.last_update + interval 1 day \
          order by s.last_update",
        &[schema],
    ) {
        Ok(rows) => {
            for row in rows {
                let name = text(&row, 0);
                let table = object(schema, &name, ObjectType::Table);
                scan.findings.push(Finding {
                    name: shown(d, schema, &name),
                    problem: "Statistics behind".to_owned(),
                    evidence: format!(
                        "updated {}, the table changed {}",
                        ago(num(&row, 1)),
                        ago(num(&row, 2))
                    ),
                    statements: statements(
                        d,
                        &table,
                        Task::Analyze,
                        &Options::default(),
                        &Facts::default(),
                    ),
                    target: None,
                    object: Some(table),
                    blocks: false,
                    picked: true,
                });
            }
        }
        Err(_) => note(
            &mut scan,
            "Statistics",
            "their age can't be read (needs SELECT on mysql.innodb_table_stats)",
            false,
        ),
    }
    Ok(scan)
}

// ----- SQL Server -----------------------------------------------------------

fn sqlserver(c: &mut dyn Client, limits: &Limits) -> Result<Scan, DbError> {
    let d = Dialect::SQLSERVER;
    let info = c.query(
        "select db_name(), cast(serverproperty('EngineEdition') as int), \
                datediff_big(second, cast(databasepropertyex(db_name(), 'LastGoodCheckDbTime') \
                                          as datetime), getdate()), \
                cast(databasepropertyex(db_name(), 'LastGoodCheckDbTime') as datetime)",
        &[],
    )?;
    let info = info.first().cloned().unwrap_or_default();
    let database = text(&info, 0);
    // Enterprise (and Developer), Azure SQL Database, Managed Instance.
    let online = matches!(int(&info, 1), Some(3 | 5 | 8));
    let mut scan = Scan {
        scope: format!("database {database}"),
        ..Scan::default()
    };

    let pages = limits.pages.to_string();
    match c.query(
        "select s.name, o.name, i.name, max(ps.avg_fragmentation_in_percent), \
                sum(ps.page_count) \
           from sys.dm_db_index_physical_stats(db_id(), null, null, null, 'LIMITED') ps \
           join sys.indexes i on i.object_id = ps.object_id and i.index_id = ps.index_id \
           join sys.objects o on o.object_id = ps.object_id \
           join sys.schemas s on s.schema_id = o.schema_id \
          where ps.index_id > 0 and ps.alloc_unit_type_desc = 'IN_ROW_DATA' \
            and o.is_ms_shipped = 0 \
          group by s.name, o.name, i.name \
         having sum(ps.page_count) >= cast(@P1 as bigint) \
            and max(ps.avg_fragmentation_in_percent) > 5 \
          order by 4 desc",
        &[&pages],
    ) {
        Ok(rows) => {
            for row in rows {
                let (schema, table, index) = (text(&row, 0), text(&row, 1), text(&row, 2));
                let pct = num(&row, 3).unwrap_or(0.0);
                let on = d.qualified(&schema, &table);
                let rebuild = pct > 30.0;
                let sql = if rebuild {
                    let with = if online { " with (online = on)" } else { "" };
                    format!(
                        "alter index {} on {on} rebuild{with}",
                        d.quote_ident(&index)
                    )
                } else {
                    format!("alter index {} on {on} reorganize", d.quote_ident(&index))
                };
                scan.findings.push(Finding {
                    object: Some(object(&schema, &table, ObjectType::Table)),
                    name: format!("{} ({index})", shown(d, &schema, &table)),
                    problem: "Fragmented index".to_owned(),
                    evidence: format!(
                        "{pct:.0} % of {} pages: {}",
                        grouped(int(&row, 4).unwrap_or(0)),
                        if rebuild {
                            "rebuild (over 30 %)"
                        } else {
                            "reorganize (5 to 30 %)"
                        }
                    ),
                    statements: vec![sql],
                    target: None,
                    blocks: rebuild && !online,
                    picked: true,
                });
            }
        }
        Err(e) => note(
            &mut scan,
            "Fragmentation",
            crate::object_editor::first_line(&e.message),
            false,
        ),
    }

    let changes = limits.changes.to_string();
    match c.query(
        "select sc.name, o.name, count(*), max(sp.modification_counter), \
                max(datediff_big(second, sp.last_updated, getdate())) \
           from sys.stats st \
           join sys.objects o on o.object_id = st.object_id \
           join sys.schemas sc on sc.schema_id = o.schema_id \
          cross apply sys.dm_db_stats_properties(st.object_id, st.stats_id) sp \
          where o.type = 'U' and o.is_ms_shipped = 0 \
            and sp.modification_counter >= cast(@P1 as bigint) \
            and sp.modification_counter > sp.rows / 5 \
          group by sc.name, o.name \
          order by 4 desc",
        &[&changes],
    ) {
        Ok(rows) => {
            for row in rows {
                let (schema, name) = (text(&row, 0), text(&row, 1));
                let table = object(&schema, &name, ObjectType::Table);
                let n = int(&row, 2).unwrap_or(0);
                scan.findings.push(Finding {
                    name: shown(d, &schema, &name),
                    problem: "Statistics behind".to_owned(),
                    evidence: format!(
                        "{n} statistic{}, up to {} rows changed since updated {}",
                        if n == 1 { "" } else { "s" },
                        grouped(int(&row, 3).unwrap_or(0)),
                        ago(num(&row, 4))
                    ),
                    statements: statements(
                        d,
                        &table,
                        Task::UpdateStatistics,
                        &Options::default(),
                        &Facts::default(),
                    ),
                    target: None,
                    object: Some(table),
                    blocks: false,
                    picked: true,
                });
            }
        }
        Err(e) => note(
            &mut scan,
            "Statistics",
            crate::object_editor::first_line(&e.message),
            false,
        ),
    }

    // The last good DBCC CHECKDB (2016 SP2 and later); 1900-01-01 is never.
    let checked = text(&info, 3);
    if !checked.is_empty() {
        let never = checked.starts_with("1900");
        let secs = if never { None } else { num(&info, 2) };
        let due = secs.is_none_or(|s| s > (limits.checkdb_days * 86400) as f64);
        note(&mut scan, "Last good CHECKDB", ago(secs), due);
        if due {
            scan.findings.push(Finding {
                object: None,
                name: format!("database {database}"),
                problem: "Consistency check due".to_owned(),
                evidence: format!(
                    "last good DBCC CHECKDB: {}; it reads the whole database",
                    ago(secs)
                ),
                statements: vec!["dbcc checkdb with no_infomsgs".to_owned()],
                target: None,
                blocks: false,
                picked: false,
            });
        }
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_oracle_names_in_lowercase() {
        assert_eq!(shown(Dialect::ORACLE, "APP", "ORDERS"), "app.orders");
        assert_eq!(shown(Dialect::POSTGRES, "app", "Orders"), "app.Orders");
    }

    #[test]
    fn titles() {
        assert_eq!(
            ScanWindow::new("APP".into()).title(),
            "app maintenance scan"
        );
        assert_eq!(ScanWindow::new(String::new()).title(), "Maintenance scan");
    }
}
