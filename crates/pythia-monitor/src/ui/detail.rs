//! Details of one session: overview, SQL, plan, waits, statistics, cursors,
//! locks and all of its `V$SESSION` row. Each tab is read when it is first
//! shown; Refresh reads the shown one again and the others when shown next.

use std::collections::HashMap;

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use pythia_db::DbError;

use super::history::HistoryView;
use super::sessions::{KillConfirm, RowAction, kill_dialog, session_actions};
use super::style::{self, Palette, duration, section};
use super::{Analysis, MonitorAction, Want, Wants};
use crate::queries::{
    Capabilities, DetailTab, Scope, SessionDetail, SessionOverview, Transaction, explain_error,
};

/// The parts the assistant's analysis is made of.
const ANALYZED: [DetailTab; 5] = [
    DetailTab::Sql,
    DetailTab::Plan,
    DetailTab::Waits,
    DetailTab::Stats,
    DetailTab::Locks,
];

pub struct SessionDetailView {
    pub sid: i64,
    pub serial: i64,
    tab: DetailTab,
    detail: Option<SessionDetail>,
    error: Option<String>,
    loading: bool,
    /// The shown tab was asked for since the last refresh.
    requested: bool,
    stat_filter: String,
    /// Statistics as of the read before, for the change since.
    prev_stats: HashMap<String, f64>,
    /// The open cursor whose statement is shown below the list.
    cursor: Option<usize>,
    /// Open cursors grouped by SQL_ID, with counts.
    group_cursors: bool,
    column_filter: String,
    /// Asking before a kill.
    confirm: Option<KillConfirm>,
    /// Analyze with AI waits for the parts it needs.
    analyze: bool,
    /// The Plan tab's tree.
    plan_view: pythia_ui::plan::PlanView,
    /// Its activity, ASH report and monitored executions.
    pub(crate) history: HistoryView,
}

impl SessionDetailView {
    pub fn new(sid: i64, serial: i64) -> Self {
        Self {
            sid,
            serial,
            tab: DetailTab::default(),
            detail: None,
            error: None,
            loading: false,
            requested: false,
            stat_filter: String::new(),
            prev_stats: HashMap::new(),
            cursor: None,
            group_cursors: false,
            column_filter: String::new(),
            confirm: None,
            analyze: false,
            plan_view: pythia_ui::plan::PlanView::default(),
            history: HistoryView::new(Scope::Session { sid, serial }),
        }
    }

    /// A copy with the same details, for a test harness of its own.
    #[cfg(test)]
    fn clone_for_test(&self) -> Self {
        let mut view = Self::new(self.sid, self.serial);
        view.requested = self.requested;
        view.detail = self.detail.clone();
        view
    }

    /// Shows one of the tabs (e.g. the open cursors of a session the
    /// dashboard lists for them).
    pub fn show_tab(&mut self, tab: DetailTab) {
        self.tab = tab;
    }

    /// Reads the details again: the shown tab with the next frame, the
    /// others when shown next (after a kill, a cancel or a trace change, or
    /// Refresh).
    pub(crate) fn refresh(&mut self) {
        if !self.loading {
            self.requested = false;
        }
        if let Some(d) = &mut self.detail {
            let shown = self.tab;
            d.loaded.retain(|t| *t == shown);
        }
        self.history.refresh();
    }

    /// Asks for the header and `parts`.
    fn request(&mut self, parts: Vec<DetailTab>, wants: &mut Wants) {
        self.loading = true;
        wants.push(Want::Detail {
            sid: self.sid,
            serial: self.serial,
            parts,
        });
    }

    /// The transaction the kill dialog asked for.
    pub(crate) fn on_transaction(&mut self, result: &Result<Option<Transaction>, String>) {
        if let Some(k) = &mut self.confirm {
            k.on_transaction(self.sid, self.serial, result);
        }
    }

    pub fn title(&self) -> String {
        format!("Session {}", self.sid)
    }

    pub(crate) fn on_detail(&mut self, result: Result<SessionDetail, DbError>) {
        self.loading = false;
        let d = match result {
            Ok(d) => d,
            Err(e) => {
                self.error = Some(explain_error(&e));
                return;
            }
        };
        self.error = None;
        let Some(old) = &mut self.detail else {
            self.detail = Some(d);
            return;
        };
        if d.loaded.contains(&DetailTab::Stats) && old.loaded.contains(&DetailTab::Stats) {
            self.prev_stats = old.stats.iter().cloned().collect();
        }
        if d.loaded.contains(&DetailTab::Cursors) {
            // Keep the selected cursor if it's still there.
            let selected = self.cursor.and_then(|i| old.cursors.get(i));
            self.cursor = selected.and_then(|c| d.cursors.iter().position(|n| n == c));
        }
        old.merge(d);
    }

    pub(crate) fn ui(&mut self, ui: &mut Ui, caps: Option<&Capabilities>, wants: &mut Wants) {
        // The shown tab, when it hasn't been read (or is to be read again);
        // nothing more of a session that is gone.
        let gone = self.detail.as_ref().is_some_and(|d| d.session.is_none());
        let loaded = |d: &Option<SessionDetail>, t: DetailTab| {
            d.as_ref().is_some_and(|d| d.loaded.contains(&t))
        };
        if !self.loading && (!self.requested || (!gone && !loaded(&self.detail, self.tab))) {
            self.requested = true;
            self.request(vec![self.tab], wants);
        }
        // Analyze with AI once what it needs is there.
        if self.analyze && !self.loading {
            let missing: Vec<DetailTab> = ANALYZED
                .into_iter()
                .filter(|t| !loaded(&self.detail, *t))
                .collect();
            match &self.detail {
                Some(d) if missing.is_empty() || gone => {
                    self.analyze = false;
                    if !gone {
                        wants.push(Want::Analyze(analysis(self.sid, self.serial, d)));
                    }
                }
                _ => self.request(missing, wants),
            }
        }
        let mut actions = Vec::new();
        let mut refresh = false;
        ui.horizontal(|ui| {
            let s = self.detail.as_ref().and_then(|d| d.session.as_ref());
            match s {
                Some(s) => {
                    ui.label(
                        RichText::new(format!(
                            "{}  {}",
                            s.handle(),
                            s.username.as_deref().unwrap_or("—")
                        ))
                        .strong()
                        .monospace(),
                    );
                    let color = if s.is_active() {
                        Palette::GREEN
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.label(RichText::new(&s.status).color(color));
                    ui.weak(format!(
                        "{} · {} · logon {} · last call {}",
                        s.program.as_deref().unwrap_or(""),
                        s.machine.as_deref().unwrap_or(""),
                        s.logon_time,
                        duration(s.last_call_et as f64)
                    ));
                }
                None if self.detail.is_some() => {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("Session {},{} no longer exists", self.sid, self.serial),
                    );
                }
                None => {
                    ui.label(
                        RichText::new(format!("Session {},{}", self.sid, self.serial)).strong(),
                    );
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(s) = self.detail.as_ref().and_then(|d| d.session.as_ref()) {
                    ui.menu_button("Session ⏷", |ui| session_actions(ui, s, caps, &mut actions))
                        .response
                        .on_hover_text("Cancel its SQL, kill or disconnect it, trace it");
                }
                if ui.button("⟳").on_hover_text("Refresh").clicked() && !self.loading {
                    refresh = true;
                }
                if self.loading {
                    ui.spinner();
                }
                if wants.analyze
                    && self.detail.as_ref().is_some_and(|d| d.session.is_some())
                    && ui
                        .button("Analyze with AI")
                        .on_hover_text("Have the AI assistant explain what the session waits for and why its SQL is slow")
                        .clicked()
                {
                    self.analyze = true;
                }
            });
        });
        if let Some(s) = self.detail.as_ref().and_then(|d| d.session.as_ref())
            && (s.is_blocked() || s.event.is_some())
        {
            ui.horizontal(|ui| {
                ui.weak("Current wait:");
                ui.label(s.event.as_deref().unwrap_or("ON CPU"));
                if s.wait_secs > 0.0 {
                    ui.weak(format!("for {}", duration(s.wait_secs)));
                }
                if let Some(b) = s.blocking_session {
                    let text = RichText::new(format!("blocked by {b}"))
                        .color(Palette::RED)
                        .strong();
                    match s.blocking_serial {
                        Some(serial) => {
                            if ui.link(text).on_hover_text("Show its details").clicked() {
                                actions.push(RowAction::Want(Want::OpenDetail {
                                    sid: b,
                                    serial,
                                    tab: None,
                                }));
                            }
                        }
                        None => {
                            ui.label(text);
                        }
                    }
                }
                if let Some(o) = &s.wait_object {
                    ui.weak(format!("object {o}"));
                }
                if let Some(r) = &s.wait_rowid {
                    match s.wait_object.as_deref().and_then(|o| o.split_once('.')) {
                        Some((owner, table)) => {
                            if ui
                                .link(format!("row {r}"))
                                .on_hover_text("Query the row in a SQL window")
                                .clicked()
                            {
                                let oracle = pythia_db::Dialect::ORACLE;
                                wants.push(Want::Act(MonitorAction::OpenSql(format!(
                                    "select * from {} where rowid = '{r}'",
                                    oracle.qualified(owner, table)
                                ))));
                            }
                        }
                        None => {
                            ui.weak(format!("row {r}"));
                        }
                    }
                }
            });
        }
        if let Some(d) = self.detail.as_ref().filter(|d| !d.blocks.is_empty()) {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Blocks").color(Palette::RED).strong());
                for (sid, serial, user) in &d.blocks {
                    let who = user.as_deref().unwrap_or("background");
                    if ui
                        .link(format!("{sid} ({who})"))
                        .on_hover_text("Show its details")
                        .clicked()
                    {
                        actions.push(RowAction::Want(Want::OpenDetail {
                            sid: *sid,
                            serial: *serial,
                            tab: None,
                        }));
                    }
                }
            });
        }
        for a in actions {
            a.apply(&mut self.confirm, wants);
        }
        if refresh {
            self.refresh();
        }
        kill_dialog(ui.ctx(), &mut self.confirm, wants);
        ui.horizontal(|ui| {
            for (t, label) in [
                (DetailTab::Overview, "Overview"),
                (DetailTab::Sql, "SQL"),
                (DetailTab::Plan, "Plan"),
                (DetailTab::Waits, "Wait history"),
                (DetailTab::Stats, "Statistics"),
                (DetailTab::Cursors, "Open cursors"),
                (DetailTab::Locks, "Locks"),
                (DetailTab::Columns, "All columns"),
                (DetailTab::History, "History"),
            ] {
                ui.selectable_value(&mut self.tab, t, label);
            }
            if self.loading {
                ui.spinner();
            }
        });
        ui.separator();
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return;
        }
        let Some(d) = &self.detail else {
            ui.spinner();
            return;
        };
        style::section_errors(ui, &d.errors);
        let h = super::style::row_height(ui);
        match self.tab {
            DetailTab::Overview => {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| match &d.overview {
                        Some(o) => overview(ui, o, d.transaction.as_ref(), wants),
                        None => {
                            ui.spinner();
                        }
                    });
            }
            DetailTab::Sql => {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let session = d.session.as_ref();
                        let statements = [
                            (
                                "Current SQL",
                                session.and_then(|s| s.sql_id.clone()),
                                &d.sql_text,
                                &d.binds,
                            ),
                            (
                                "Previous SQL",
                                session.and_then(|s| s.prev_sql_id.clone()),
                                &d.prev_sql_text,
                                &d.prev_binds,
                            ),
                        ];
                        for (title, sql_id, text, binds) in statements {
                            let title = format!("{title} {}", sql_id.as_deref().unwrap_or(""));
                            section(ui, &title, |ui| {
                                code(ui, text.as_deref());
                                if let Some(id) = sql_id
                                    && ui
                                        .small_button("SQL details")
                                        .on_hover_text(
                                            "Totals, child cursors, plans, sessions running it",
                                        )
                                        .clicked()
                                {
                                    wants.push(Want::OpenSqlDetail { sql_id: id });
                                }
                                if let Some(text) = text
                                    && ui
                                        .small_button("Open in SQL window")
                                        .on_hover_text("Run it, explain it, tune it")
                                        .clicked()
                                {
                                    wants.push(Want::Act(MonitorAction::OpenSql(text.clone())));
                                }
                                if !binds.is_empty() {
                                    ui.add_space(4.0);
                                    ui.label(RichText::new("Captured binds").strong());
                                    egui::Grid::new(("binds", title.as_str()))
                                        .striped(true)
                                        .show(ui, |ui| {
                                            for b in binds {
                                                let name = b
                                                    .name
                                                    .clone()
                                                    .unwrap_or_else(|| format!(":{}", b.position));
                                                ui.label(RichText::new(name).monospace());
                                                ui.weak(&b.data_type);
                                                ui.label(
                                                    RichText::new(
                                                        b.value.as_deref().unwrap_or("NULL"),
                                                    )
                                                    .monospace(),
                                                );
                                                ui.weak(b.captured.as_deref().unwrap_or(""));
                                                ui.end_row();
                                            }
                                        });
                                }
                            });
                            ui.add_space(6.0);
                        }
                        ui.weak(
                            "Binds are sampled at most every 15 minutes, and only those in \
                             WHERE and HAVING clauses.",
                        );
                    });
            }
            DetailTab::Plan => {
                if d.plan.is_empty() {
                    ui.weak("No plan available (the session has no current or previous cursor).");
                } else {
                    self.plan_view.report_ui(
                        ui,
                        &d.plan_rows,
                        &d.plan.join("\n"),
                        egui::Id::new(("session-plan", self.sid, self.serial)),
                    );
                }
            }
            DetailTab::Waits => {
                table(
                    ui,
                    "detail-waits",
                    h,
                    &["#", "Event", "Wait (ms)", "Parameters"],
                    d.waits.len(),
                    |row, i| {
                        let w = &d.waits[i];
                        row.col(|ui| {
                            ui.label(w.seq.to_string());
                        });
                        row.col(|ui| {
                            ui.label(&w.event);
                        });
                        row.col(|ui| {
                            ui.label(format!("{:.2}", w.wait_ms));
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(&w.params).monospace().small());
                        });
                    },
                );
            }
            DetailTab::Stats => {
                ui.add(
                    egui::TextEdit::singleline(&mut self.stat_filter)
                        .hint_text("Filter statistics…"),
                );
                let f = self.stat_filter.to_lowercase();
                let stats: Vec<&(String, f64)> = d
                    .stats
                    .iter()
                    .filter(|(n, _)| n.to_lowercase().contains(&f))
                    .collect();
                let prev = &self.prev_stats;
                table(
                    ui,
                    "detail-stats",
                    h,
                    &["Statistic", "Value", "Change since the read before"],
                    stats.len(),
                    |row, i| {
                        let (n, v) = stats[i];
                        row.col(|ui| {
                            ui.label(n);
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(format!("{v}")).monospace());
                        });
                        row.col(|ui| {
                            let change = v - prev.get(n).copied().unwrap_or(0.0);
                            if !prev.is_empty() && change != 0.0 {
                                ui.label(
                                    RichText::new(format!("{change:+}"))
                                        .monospace()
                                        .color(Palette::ORANGE),
                                );
                            }
                        });
                    },
                );
            }
            DetailTab::Cursors if self.group_cursors => {
                ui.checkbox(&mut self.group_cursors, "Group by SQL_ID");
                // The same statement open many times hints at a cursor
                // leak.
                let mut groups: Vec<(Option<&str>, usize, &str)> = Vec::new();
                for c in &d.cursors {
                    match groups.iter_mut().find(|g| g.0 == c.sql_id.as_deref()) {
                        Some(g) => g.1 += 1,
                        None => groups.push((c.sql_id.as_deref(), 1, &c.text)),
                    }
                }
                groups.sort_by_key(|g| std::cmp::Reverse(g.1));
                table(
                    ui,
                    "detail-cursor-groups",
                    h,
                    &["SQL_ID", "Open", "SQL text"],
                    groups.len(),
                    |row, i| {
                        let (sql_id, n, text) = groups[i];
                        row.col(|ui| match sql_id {
                            Some(id) => {
                                if ui
                                    .link(RichText::new(id).monospace())
                                    .on_hover_text("Show the statement")
                                    .clicked()
                                {
                                    wants.push(Want::OpenSqlDetail {
                                        sql_id: id.to_owned(),
                                    });
                                }
                            }
                            None => {
                                ui.label("");
                            }
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(n.to_string()).monospace());
                        });
                        row.col(|ui| {
                            let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
                            ui.add(
                                egui::Label::new(RichText::new(line).monospace().small())
                                    .truncate(),
                            );
                        });
                    },
                );
            }
            DetailTab::Cursors => {
                ui.checkbox(&mut self.group_cursors, "Group by SQL_ID");
                // The selected cursor's whole statement below the list.
                egui::Panel::bottom(egui::Id::new(("detail-cursor-text", self.sid)))
                    .resizable(true)
                    .default_size(140.0)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink(false)
                            .show(ui, |ui| match self.cursor.and_then(|i| d.cursors.get(i)) {
                                Some(c) => code(ui, Some(&c.text)),
                                None => {
                                    ui.weak("Select a cursor to see its statement");
                                }
                            });
                    });
                let mut clicked = None;
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        TableBuilder::new(ui)
                            .id_salt("detail-cursors")
                            .striped(true)
                            .resizable(true)
                            .auto_shrink(false)
                            .sense(egui::Sense::click())
                            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                            .column(Column::initial(130.0).clip(true))
                            .column(Column::initial(130.0).clip(true))
                            .column(Column::remainder().clip(true))
                            .header(h, |mut r| {
                                for title in ["SQL_ID", "Type", "SQL text"] {
                                    r.col(|ui| {
                                        ui.strong(title);
                                    });
                                }
                            })
                            .body(|body| {
                                body.rows(h, d.cursors.len(), |mut row| {
                                    let i = row.index();
                                    let c = &d.cursors[i];
                                    row.set_selected(self.cursor == Some(i));
                                    row.col(|ui| {
                                        if let Some(id) = &c.sql_id
                                            && ui
                                                .link(RichText::new(id).monospace())
                                                .on_hover_text("Show the statement")
                                                .clicked()
                                        {
                                            wants.push(Want::OpenSqlDetail { sql_id: id.clone() });
                                        }
                                    });
                                    row.col(|ui| {
                                        ui.label(c.cursor_type.as_deref().unwrap_or(""));
                                    });
                                    row.col(|ui| {
                                        let line =
                                            c.text.split_whitespace().collect::<Vec<_>>().join(" ");
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(line).monospace().small(),
                                            )
                                            .truncate()
                                            .show_tooltip_when_elided(false)
                                            .selectable(false),
                                        );
                                    });
                                    if row.response().clicked() {
                                        clicked = Some(i);
                                    }
                                });
                            });
                    });
                if clicked.is_some() {
                    self.cursor = clicked;
                }
            }
            DetailTab::Locks => {
                table(
                    ui,
                    "detail-locks",
                    h,
                    &[
                        "Type",
                        "Object / ID1",
                        "ID2",
                        "Held",
                        "Requested",
                        "Seconds",
                        "Blocking",
                    ],
                    d.locks.len(),
                    |row, i| {
                        let l = &d.locks[i];
                        row.col(|ui| {
                            ui.label(RichText::new(&l.lock_type).monospace());
                        });
                        row.col(
                            |ui| match l.object.as_deref().and_then(|o| o.split_once('.')) {
                                Some((owner, name)) => {
                                    if ui
                                        .link(format!("{owner}.{name}"))
                                        .on_hover_text("Show the table")
                                        .clicked()
                                    {
                                        wants.push(Want::Act(MonitorAction::OpenObject {
                                            owner: owner.to_owned(),
                                            name: name.to_owned(),
                                            object_type: "TABLE".into(),
                                            subprogram: None,
                                        }));
                                    }
                                }
                                None => {
                                    ui.label(l.id1.to_string());
                                }
                            },
                        );
                        row.col(|ui| {
                            ui.label(l.id2.to_string());
                        });
                        row.col(|ui| {
                            ui.label(&l.mode_held);
                        });
                        row.col(|ui| {
                            let color = if l.mode_requested != "None" {
                                Palette::RED
                            } else {
                                ui.visuals().weak_text_color()
                            };
                            ui.label(RichText::new(&l.mode_requested).color(color));
                        });
                        row.col(|ui| {
                            ui.label(duration(l.seconds as f64));
                        });
                        row.col(|ui| {
                            if l.blocking {
                                ui.label(RichText::new("blocking").color(Palette::RED).strong());
                            }
                        });
                    },
                );
            }
            DetailTab::History => {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| self.history.ui(ui, caps, wants));
            }
            DetailTab::Columns => {
                ui.add(
                    egui::TextEdit::singleline(&mut self.column_filter)
                        .hint_text("Filter columns…"),
                );
                let f = self.column_filter.to_lowercase();
                let columns: Vec<&(String, String)> = d
                    .columns
                    .iter()
                    .filter(|(n, v)| n.to_lowercase().contains(&f) || v.to_lowercase().contains(&f))
                    .collect();
                table(
                    ui,
                    "detail-columns",
                    h,
                    &["Column", "Value"],
                    columns.len(),
                    |row, i| {
                        let (n, v) = columns[i];
                        row.col(|ui| {
                            ui.label(RichText::new(n).monospace());
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(v).monospace());
                        });
                    },
                );
            }
        }
    }
}

/// The Overview tab: who the session is, what it holds and where its time
/// went.
fn overview(
    ui: &mut Ui,
    o: &SessionOverview,
    transaction: Option<&Transaction>,
    wants: &mut Wants,
) {
    let pairs = |ui: &mut Ui, id: &str, rows: &[(String, String)]| {
        egui::Grid::new(id).num_columns(2).show(ui, |ui| {
            for (name, value) in rows {
                ui.weak(name);
                ui.label(value);
                ui.end_row();
            }
        });
    };
    ui.columns(2, |cols| {
        section(&mut cols[0], "Client", |ui| {
            pairs(ui, "overview-client", &o.client)
        });
        section(&mut cols[1], "Connection", |ui| {
            pairs(ui, "overview-connection", &o.connection)
        });
    });
    ui.add_space(6.0);
    ui.columns(2, |cols| {
        section(&mut cols[0], "Transaction", |ui| match transaction {
            Some(t) => {
                pairs(
                    ui,
                    "overview-transaction",
                    &[
                        ("Open since".into(), t.start.clone()),
                        ("Status".into(), t.status.clone()),
                        (
                            "Undo".into(),
                            format!("{} records in {} blocks", t.undo_records, t.undo_blocks),
                        ),
                    ],
                );
            }
            None => {
                ui.weak("No open transaction");
            }
        });
        section(&mut cols[1], "Memory and temp space", |ui| {
            let mut rows = Vec::new();
            if let Some(p) = &o.process {
                rows.push(("Server process".into(), p.spid.clone()));
                rows.push((
                    "PGA".into(),
                    format!(
                        "{} used · {} allocated · {} at most",
                        style::bytes(p.pga_used),
                        style::bytes(p.pga_allocated),
                        style::bytes(p.pga_max)
                    ),
                ));
            }
            for t in &o.temp {
                rows.push((
                    format!("Temp {}", t.segment.to_lowercase()),
                    format!("{} in {}", style::bytes(t.bytes), t.tablespace),
                ));
            }
            for w in &o.workareas {
                rows.push((
                    w.operation.to_lowercase(),
                    format!(
                        "{} memory{}{}",
                        style::bytes(w.memory),
                        if w.temp > 0.0 {
                            format!(" · {} temp", style::bytes(w.temp))
                        } else {
                            String::new()
                        },
                        w.sql_id
                            .as_deref()
                            .map(|id| format!(" · {id}"))
                            .unwrap_or_default()
                    ),
                ));
            }
            if rows.is_empty() {
                ui.weak("—");
            }
            pairs(ui, "overview-memory", &rows);
        });
    });
    if !o.plsql.is_empty() {
        ui.add_space(6.0);
        section(ui, "PL/SQL", |ui| {
            for u in &o.plsql {
                ui.horizontal(|ui| {
                    ui.weak(u.role);
                    let name = match &u.subprogram {
                        Some(sub) => format!("{}.{}.{}", u.owner, u.name, sub),
                        None => format!("{}.{}", u.owner, u.name),
                    }
                    .to_lowercase();
                    if ui.link(name).on_hover_text("Open it").clicked() {
                        wants.push(Want::Act(MonitorAction::OpenObject {
                            owner: u.owner.clone(),
                            name: u.name.clone(),
                            object_type: u.object_type.clone(),
                            subprogram: u.subprogram.clone(),
                        }));
                    }
                    ui.weak(u.object_type.to_lowercase());
                });
            }
        });
    }
    if !o.long_ops.is_empty() {
        ui.add_space(6.0);
        section(ui, "Long operations", |ui| {
            for op in &o.long_ops {
                let fraction = if op.totalwork > 0.0 {
                    op.sofar / op.totalwork
                } else {
                    0.0
                };
                style::usage_bar(
                    ui,
                    fraction as f32,
                    Palette::BLUE,
                    format!(
                        "{:.0} %  {} {}{}",
                        fraction * 100.0,
                        op.opname,
                        op.target.as_deref().unwrap_or(""),
                        op.remaining_s
                            .filter(|s| *s > 0)
                            .map(|s| format!(" · {} left", duration(s as f64)))
                            .unwrap_or_default()
                    ),
                );
                ui.add_space(2.0);
            }
        });
    }
    ui.add_space(6.0);
    ui.columns(2, |cols| {
        section(&mut cols[0], "Where the time went (since logon)", |ui| {
            let total = o
                .time_model
                .iter()
                .find(|(n, _)| n == "DB time")
                .map_or(0.0, |(_, v)| *v)
                .max(0.000_001);
            for (name, seconds) in o.time_model.iter().take(10) {
                style::usage_bar(
                    ui,
                    (*seconds / total) as f32,
                    Palette::TEAL,
                    format!("{}  {name}", duration(*seconds)),
                );
                ui.add_space(2.0);
            }
            if o.time_model.is_empty() {
                ui.weak("—");
            }
        });
        section(&mut cols[1], "Waits since logon", |ui| {
            egui::Grid::new("overview-events")
                .striped(true)
                .show(ui, |ui| {
                    for e in &o.events {
                        ui.label(&e.event);
                        ui.weak(&e.wait_class);
                        ui.label(format!("{} waits", e.waits));
                        ui.label(duration(e.seconds));
                        ui.weak(format!("{:.2} ms avg", e.average_ms));
                        ui.end_row();
                    }
                });
            if o.events.is_empty() {
                ui.weak("No waits besides idle ones");
            }
        });
    });
}

fn code(ui: &mut Ui, text: Option<&str>) {
    match text {
        Some(t) => {
            ui.add(egui::Label::new(RichText::new(t).monospace()).wrap());
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(t.to_owned());
            }
        }
        None => {
            ui.weak("—");
        }
    }
}

fn table(
    ui: &mut Ui,
    id: &str,
    h: f32,
    headers: &[&str],
    rows: usize,
    mut add: impl FnMut(&mut egui_extras::TableRow<'_, '_>, usize),
) {
    let mut t = TableBuilder::new(ui)
        .id_salt(id)
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
    for (i, _) in headers.iter().enumerate() {
        t = if i + 1 == headers.len() {
            t.column(Column::remainder().clip(true))
        } else {
            t.column(Column::initial(130.0).clip(true))
        };
    }
    t.header(h, |mut r| {
        for title in headers {
            r.col(|ui| {
                ui.strong(*title);
            });
        }
    })
    .body(|body| {
        body.rows(h, rows, |mut row| {
            let i = row.index();
            add(&mut row, i);
        })
    });
}

/// A session's state, SQL, plan, waits, busiest statistics and locks, for
/// the assistant.
fn analysis(sid: i64, serial: i64, d: &SessionDetail) -> Analysis {
    let mut details = String::new();
    if let Some(s) = &d.session {
        details.push_str(&format!(
            "Session {sid},{serial}: user {}, program {}, module {}, status {}, \
             last call {} s ago\n",
            s.username.as_deref().unwrap_or("-"),
            s.program.as_deref().unwrap_or("-"),
            s.module.as_deref().unwrap_or("-"),
            s.status,
            s.last_call_et
        ));
        match &s.event {
            Some(event) => details.push_str(&format!(
                "Current wait: {event} ({}) for {:.1} s{}{}\n",
                s.wait_class.as_deref().unwrap_or("-"),
                s.wait_secs,
                s.blocking_session
                    .map(|b| format!(", blocked by session {b}"))
                    .unwrap_or_default(),
                s.wait_object
                    .as_ref()
                    .map(|o| format!(", on object {o}"))
                    .unwrap_or_default(),
            )),
            None => details.push_str("On CPU\n"),
        }
        if let Some(prev) = s.prev_sql_id.as_ref().filter(|_| d.sql_text.is_none()) {
            details.push_str(&format!("No current SQL; previous SQL_ID {prev}\n"));
        }
    }
    if !d.plan.is_empty() {
        details.push_str(&format!(
            "\nPlan of the current SQL:\n{}\n",
            d.plan.join("\n")
        ));
    }
    if !d.waits.is_empty() {
        details.push_str("\nRecent waits (newest first):\n");
        for w in &d.waits {
            details.push_str(&format!("{} {:.2} ms {}\n", w.event, w.wait_ms, w.params));
        }
    }
    let mut stats: Vec<&(String, f64)> = d.stats.iter().filter(|(_, v)| *v != 0.0).collect();
    stats.sort_by(|a, b| b.1.total_cmp(&a.1));
    if !stats.is_empty() {
        details.push_str("\nSession statistics (largest 40):\n");
        for (name, value) in stats.into_iter().take(40) {
            details.push_str(&format!("{name}: {value}\n"));
        }
    }
    if !d.locks.is_empty() {
        details.push_str("\nLocks:\n");
        for l in &d.locks {
            details.push_str(&format!(
                "{} on {} held {} requested {} for {} s{}\n",
                l.lock_type,
                l.object.clone().unwrap_or_else(|| l.id1.to_string()),
                l.mode_held,
                l.mode_requested,
                l.seconds,
                if l.blocking { ", blocking others" } else { "" }
            ));
        }
    }
    let sql_id = d.session.as_ref().and_then(|s| s.sql_id.clone());
    // The binds of the statement the analysis is about.
    let binds = if d.sql_text.is_some() {
        &d.binds
    } else {
        &d.prev_binds
    };
    Analysis {
        title: format!("session {sid},{serial}"),
        sql: d.sql_text.clone().or_else(|| d.prev_sql_text.clone()),
        // The detail has the plan already.
        sql_id: sql_id.filter(|_| d.plan.is_empty()),
        details,
        binds: binds
            .iter()
            .map(|b| {
                let name = b.name.clone().unwrap_or_else(|| format!(":{}", b.position));
                format!("{name} = {}", b.value.as_deref().unwrap_or("NULL"))
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::queries::{OpenCursor, SessionRow};
    use crate::ui::Want;

    /// The header links to the session it waits for and to those waiting
    /// for it (#74).
    #[test]
    fn blocking_links() {
        let mut view = SessionDetailView::new(42, 7);
        view.requested = true;
        view.on_detail(Ok(SessionDetail {
            session: Some(SessionRow {
                sid: 42,
                serial: 7,
                status: "ACTIVE".into(),
                event: Some("enq: TX - row lock contention".into()),
                blocking_session: Some(21),
                blocking_serial: Some(4),
                ..SessionRow::default()
            }),
            blocks: vec![(55, 8, Some("HR".into()))],
            loaded: DetailTab::ALL.to_vec(),
            overview: Some(SessionOverview::default()),
            ..SessionDetail::default()
        }));
        let opened = |label: &str| {
            let mut harness = Harness::builder()
                .with_size(egui::vec2(900.0, 500.0))
                .build_ui_state(
                    |ui, (view, wants): &mut (SessionDetailView, Wants)| view.ui(ui, None, wants),
                    (view.clone_for_test(), Wants::default()),
                );
            harness.run_steps(2);
            harness.get_by_label(label).click();
            harness.run_steps(2);
            harness
                .state()
                .1
                .list
                .iter()
                .filter_map(|w| match w {
                    Want::OpenDetail { sid, serial, .. } => Some((*sid, *serial)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(opened("blocked by 21"), [(21, 4)]);
        assert_eq!(opened("55 (HR)"), [(55, 8)]);
    }

    /// Clicking an open cursor shows its whole statement below the list
    /// (#73).
    #[test]
    fn open_cursor_statement_below_the_list() {
        let text = "select id,\n       name\n  from a_table_with_a_long_name\n where id = :1";
        let cursor = |sql_id: &str, text: &str| OpenCursor {
            sql_id: Some(sql_id.into()),
            text: text.into(),
            cursor_type: Some("OPEN".into()),
        };
        let mut view = SessionDetailView::new(42, 7);
        view.tab = DetailTab::Cursors;
        // Loaded already: no request, no spinner.
        view.requested = true;
        view.on_detail(Ok(SessionDetail {
            session: Some(SessionRow {
                sid: 42,
                serial: 7,
                status: "INACTIVE".into(),
                ..SessionRow::default()
            }),
            cursors: vec![cursor("a1", "select 1 from dual"), cursor("b2", text)],
            loaded: vec![DetailTab::Cursors],
            ..SessionDetail::default()
        }));
        let mut harness = Harness::builder()
            .with_size(egui::vec2(900.0, 500.0))
            .build_ui_state(
                |ui, view: &mut SessionDetailView| view.ui(ui, None, &mut Wants::default()),
                view,
            );
        harness.run();
        // The list shows the statement on one line; the whole text only
        // once selected.
        assert!(harness.query_by_label(text).is_none());
        harness
            .get_by_label("select id, name from a_table_with_a_long_name where id = :1")
            .click();
        harness.run();
        assert!(harness.query_by_label(text).is_some());
        assert_eq!(harness.state().cursor, Some(1));
        // A refresh keeps the selection where the cursor is now.
        let mut detail = harness.state().detail.clone().unwrap();
        detail.cursors.insert(0, cursor("00", "begin null; end;"));
        harness.state_mut().on_detail(Ok(detail));
        harness.run();
        assert_eq!(harness.state().cursor, Some(2));
        assert!(harness.query_by_label(text).is_some());
    }
}
