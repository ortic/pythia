//! Details of one statement in the shared pool: its text, totals, child
//! cursors with their plans, binds and why they aren't shared, and the
//! sessions running it.

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use pythia_db::DbError;

use super::history::HistoryView;
use super::style::{self, compact, duration, section};
use super::{Analysis, MonitorAction, Want, Wants};
use crate::queries::{Capabilities, Scope, SqlDetail, explain_error};

pub struct SqlDetailView {
    pub sql_id: String,
    detail: Option<SqlDetail>,
    error: Option<String>,
    loading: bool,
    requested: bool,
    /// The child cursor asked for (else the most executed).
    child: Option<i64>,
    /// The child's plan as a tree.
    plan_view: pythia_ui::plan::PlanView,
    /// Its activity, plan history and monitored executions.
    pub(crate) history: HistoryView,
}

impl SqlDetailView {
    pub fn new(sql_id: String) -> Self {
        Self {
            history: HistoryView::new(Scope::Sql(sql_id.clone())),
            sql_id,
            detail: None,
            error: None,
            loading: false,
            requested: false,
            child: None,
            plan_view: pythia_ui::plan::PlanView::default(),
        }
    }

    /// The statement's details once read, or why they couldn't be.
    pub fn detail(&self) -> Result<Option<&SqlDetail>, &str> {
        match &self.error {
            Some(e) => Err(e),
            None => Ok(self.detail.as_ref()),
        }
    }

    /// Reads the details again with the next frame, as *Refresh* does
    /// (unless they are being read).
    pub fn reload(&mut self) {
        if !self.loading {
            self.requested = false;
        }
    }

    pub fn title(&self) -> String {
        format!("SQL {}", self.sql_id)
    }

    pub(crate) fn on_detail(&mut self, result: Result<SqlDetail, DbError>) {
        self.loading = false;
        match result {
            Ok(d) => {
                self.error = None;
                self.child = d.child;
                self.detail = Some(d);
            }
            Err(e) => self.error = Some(explain_error(&e)),
        }
    }

    fn request(&mut self, wants: &mut Wants) {
        self.requested = true;
        self.loading = true;
        wants.push(Want::SqlDetail {
            sql_id: self.sql_id.clone(),
            child: self.child,
        });
    }

    pub(crate) fn ui(&mut self, ui: &mut Ui, caps: Option<&Capabilities>, wants: &mut Wants) {
        if !self.requested && !self.loading {
            self.request(wants);
        }
        let mut reload = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.title()).strong().monospace());
            if let Some(s) = self.detail.as_ref().and_then(|d| d.stats.as_ref()) {
                ui.weak(format!(
                    "{} executions · {} elapsed · last active {}",
                    compact(s.executions),
                    duration(s.elapsed),
                    s.last_active.as_deref().unwrap_or("—")
                ));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳").on_hover_text("Refresh").clicked() && !self.loading {
                    reload = true;
                }
                if self.loading {
                    ui.spinner();
                }
                let text = self.detail.as_ref().and_then(|d| d.text.clone());
                if let Some(text) = &text {
                    if wants.analyze
                        && ui
                            .button("Analyze with AI")
                            .on_hover_text("Have the AI assistant explain why it is expensive")
                            .clicked()
                        && let Some(d) = &self.detail
                    {
                        wants.push(Want::Analyze(analysis(d)));
                    }
                    if ui
                        .button("Open in SQL window")
                        .on_hover_text("Run it, explain it, tune it")
                        .clicked()
                    {
                        wants.push(Want::Act(MonitorAction::OpenSql(text.clone())));
                    }
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(text.clone());
                    }
                }
            });
        });
        ui.separator();
        if reload {
            self.request(wants);
            self.history.refresh();
        }
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return;
        }
        let Some(d) = &self.detail else {
            ui.spinner();
            return;
        };
        style::section_errors(ui, &d.errors);
        if d.text.is_none() && d.children.is_empty() {
            ui.weak("The statement is no longer in the shared pool.");
            return;
        }
        let mut child = None;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                section(ui, "Statement", |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(d.text.as_deref().unwrap_or("—")).monospace(),
                        )
                        .wrap(),
                    );
                });
                ui.add_space(6.0);
                if let Some(s) = &d.stats {
                    section(ui, "Since it was loaded", |ui| totals(ui, s));
                    ui.add_space(6.0);
                }
                section(ui, "Child cursors", |ui| {
                    child = children(ui, d);
                });
                ui.add_space(6.0);
                if let Some(c) = d.child {
                    section(ui, &format!("Plan of child {c}"), |ui| {
                        if d.plan.is_empty() {
                            ui.weak("No plan in the cursor cache");
                            return;
                        }
                        // The page scrolls: the plan gets the height its
                        // lines (or the report's) take, up to a limit, with
                        // room for a line's predicates.
                        let row = pythia_ui::row_height(ui) + 2.0;
                        let shown = if self.plan_view.shows_text() || d.plan_rows.is_empty() {
                            ui.text_style_height(&egui::TextStyle::Monospace)
                                * (d.plan.len() as f32 + 1.0)
                        } else {
                            row * (d.plan_rows.len() as f32 + 1.0) + 100.0
                        };
                        let height = shown.min(560.0) + row;
                        ui.allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
                            self.plan_view.report_ui(
                                ui,
                                &d.plan_rows,
                                &d.plan.join("\n"),
                                egui::Id::new(("sql-detail-plan", &d.sql_id, c)),
                            );
                        });
                    });
                    ui.add_space(6.0);
                    if !d.binds.is_empty() {
                        section(ui, &format!("Captured binds of child {c}"), |ui| {
                            egui::Grid::new("sql-detail-binds")
                                .striped(true)
                                .show(ui, |ui| {
                                    for b in &d.binds {
                                        let name = b
                                            .name
                                            .clone()
                                            .unwrap_or_else(|| format!(":{}", b.position));
                                        ui.label(RichText::new(name).monospace());
                                        ui.weak(&b.data_type);
                                        ui.label(
                                            RichText::new(b.value.as_deref().unwrap_or("NULL"))
                                                .monospace(),
                                        );
                                        ui.weak(b.captured.as_deref().unwrap_or(""));
                                        ui.end_row();
                                    }
                                });
                        });
                        ui.add_space(6.0);
                    }
                    if !d.not_shared.is_empty() {
                        section(ui, &format!("Why child {c} isn't shared"), |ui| {
                            for reason in &d.not_shared {
                                ui.label(format!("• {reason}"));
                            }
                        });
                        ui.add_space(6.0);
                    }
                }
                let history = &mut self.history;
                egui::CollapsingHeader::new(
                    RichText::new("History: activity, plan history, SQL Monitor").strong(),
                )
                .id_salt("sql-history")
                .show(ui, |ui| history.ui(ui, caps, wants));
                ui.add_space(6.0);
                section(ui, "Sessions running it now", |ui| {
                    if d.sessions.is_empty() {
                        ui.weak("None");
                    }
                    ui.horizontal_wrapped(|ui| {
                        for (sid, serial, user, status) in &d.sessions {
                            let who = user.as_deref().unwrap_or("background");
                            if ui
                                .link(format!("{sid} ({who}, {})", status.to_lowercase()))
                                .on_hover_text("Show the session")
                                .clicked()
                            {
                                wants.push(Want::OpenDetail {
                                    sid: *sid,
                                    serial: *serial,
                                    tab: None,
                                });
                            }
                        }
                    });
                });
            });
        if let Some(c) = child
            && Some(c) != self.child
        {
            self.child = Some(c);
            self.request(wants);
        }
    }
}

/// Totals and per execution, side by side.
fn totals(ui: &mut Ui, s: &crate::queries::SqlStats) {
    let per = |v: f64| {
        if s.executions > 0.0 {
            v / s.executions
        } else {
            0.0
        }
    };
    egui::Grid::new("sql-detail-totals")
        .num_columns(3)
        .striped(true)
        .show(ui, |ui| {
            ui.weak("");
            ui.strong("Total");
            ui.strong("Per execution");
            ui.end_row();
            let times = [
                ("Elapsed", s.elapsed),
                ("CPU", s.cpu),
                ("User I/O wait", s.user_io_wait),
                ("Concurrency wait", s.concurrency_wait),
                ("Application wait", s.application_wait),
                ("PL/SQL", s.plsql),
            ];
            for (name, v) in times {
                ui.weak(name);
                ui.label(duration(v));
                ui.label(duration(per(v)));
                ui.end_row();
            }
            let counts = [
                ("Executions", s.executions),
                ("Buffer gets", s.buffer_gets),
                ("Disk reads", s.disk_reads),
                ("Rows", s.rows),
                ("Fetches", s.fetches),
                ("Parse calls", s.parse_calls),
                ("Sorts", s.sorts),
            ];
            for (name, v) in counts {
                ui.weak(name);
                ui.label(compact(v));
                if name == "Executions" {
                    ui.label("");
                } else {
                    ui.label(format!("{:.1}", per(v)));
                }
                ui.end_row();
            }
        });
}

/// The child cursors; returns the one clicked.
fn children(ui: &mut Ui, d: &SqlDetail) -> Option<i64> {
    let h = super::style::row_height(ui);
    // A click on a cell selects the row, not text.
    ui.style_mut().interaction.selectable_labels = false;
    let mut clicked = None;
    TableBuilder::new(ui)
        .id_salt("sql-detail-children")
        .striped(true)
        .vscroll(false)
        .sense(egui::Sense::click())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(50.0))
        .column(Column::exact(100.0))
        .column(Column::exact(80.0))
        .column(Column::exact(80.0))
        .column(Column::exact(90.0))
        .column(Column::exact(80.0))
        .column(Column::exact(110.0))
        .column(Column::remainder().clip(true))
        .header(h, |mut r| {
            for t in [
                "Child",
                "Plan hash",
                "Execs",
                "Elapsed",
                "Buffer gets",
                "Rows",
                "Schema",
                "Notes",
            ] {
                r.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(h, d.children.len(), |mut row| {
                let c = &d.children[row.index()];
                row.set_selected(d.child == Some(c.child));
                row.col(|ui| {
                    ui.label(c.child.to_string());
                });
                row.col(|ui| {
                    ui.label(RichText::new(c.plan_hash.to_string()).monospace());
                });
                row.col(|ui| {
                    ui.label(compact(c.executions));
                });
                row.col(|ui| {
                    ui.label(duration(c.elapsed));
                });
                row.col(|ui| {
                    ui.label(compact(c.buffer_gets));
                });
                row.col(|ui| {
                    ui.label(compact(c.rows));
                });
                row.col(|ui| {
                    ui.label(c.parsing_schema.as_deref().unwrap_or(""));
                });
                row.col(|ui| {
                    let mut notes = Vec::new();
                    if c.bind_aware {
                        notes.push("bind aware".to_owned());
                    } else if c.bind_sensitive {
                        notes.push("bind sensitive".to_owned());
                    }
                    for (what, name) in [
                        ("profile", &c.profile),
                        ("baseline", &c.baseline),
                        ("patch", &c.patch),
                    ] {
                        if let Some(name) = name {
                            notes.push(format!("{what} {name}"));
                        }
                    }
                    ui.weak(notes.join(" · "));
                });
                if row.response().clicked() {
                    clicked = Some(c.child);
                }
            });
        });
    clicked
}

/// The statement, its totals, children and plan, for the assistant.
fn analysis(d: &SqlDetail) -> Analysis {
    let mut details = String::new();
    if let Some(s) = &d.stats {
        details.push_str(&format!(
            "Since loaded: {} executions, elapsed {:.3} s, CPU {:.3} s, buffer gets {}, \
             disk reads {}, rows {}, user I/O wait {:.3} s, concurrency wait {:.3} s\n",
            s.executions,
            s.elapsed,
            s.cpu,
            s.buffer_gets,
            s.disk_reads,
            s.rows,
            s.user_io_wait,
            s.concurrency_wait
        ));
    }
    if d.children.len() > 1 {
        details.push_str(&format!("{} child cursors:\n", d.children.len()));
        for c in &d.children {
            details.push_str(&format!(
                "child {} plan {} executions {} elapsed {:.3} s buffer gets {}{}\n",
                c.child,
                c.plan_hash,
                c.executions,
                c.elapsed,
                c.buffer_gets,
                if c.bind_aware { " (bind aware)" } else { "" }
            ));
        }
        if !d.not_shared.is_empty() {
            details.push_str(&format!(
                "Not shared because: {}\n",
                d.not_shared.join("; ")
            ));
        }
    }
    if !d.plan.is_empty() {
        details.push_str(&format!(
            "\nPlan of child {}:\n{}\n",
            d.child.unwrap_or(0),
            d.plan.join("\n")
        ));
    }
    Analysis {
        title: format!("SQL {}", d.sql_id),
        sql: d.text.clone(),
        sql_id: d.plan.is_empty().then(|| d.sql_id.clone()),
        details,
        binds: d
            .binds
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
    use crate::queries::{ChildCursor, SqlStats};

    /// Clicking a child cursor reads its plan and binds; a session running
    /// the statement opens its details; the statement opens in a SQL
    /// window (#74).
    #[test]
    fn links() {
        let mut view = SqlDetailView::new("7h35uxf5uhmm1".into());
        view.requested = true;
        view.on_detail(Ok(SqlDetail {
            sql_id: "7h35uxf5uhmm1".into(),
            text: Some("select 1 from dual".into()),
            stats: Some(SqlStats {
                executions: 2.0,
                ..SqlStats::default()
            }),
            children: vec![
                ChildCursor {
                    child: 0,
                    plan_hash: 111,
                    ..ChildCursor::default()
                },
                ChildCursor {
                    child: 1,
                    plan_hash: 222,
                    ..ChildCursor::default()
                },
            ],
            child: Some(0),
            sessions: vec![(42, 7, Some("SCOTT".into()), "ACTIVE".into())],
            ..SqlDetail::default()
        }));
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 900.0))
            .build_ui_state(
                |ui, (view, wants): &mut (SqlDetailView, Wants)| view.ui(ui, None, wants),
                (view, Wants::default()),
            );
        harness.run_steps(2);
        let wants = |h: &mut Harness<'_, (SqlDetailView, Wants)>| -> Vec<String> {
            std::mem::take(&mut h.state_mut().1.list)
                .iter()
                .map(|w| match w {
                    Want::SqlDetail { sql_id, child } => format!("read {sql_id} {child:?}"),
                    Want::OpenDetail { sid, serial, .. } => format!("session {sid},{serial}"),
                    Want::Act(MonitorAction::OpenSql(text)) => format!("open {text}"),
                    _ => "other".into(),
                })
                .collect()
        };
        harness.get_by_label("222").click();
        harness.run_steps(2);
        assert_eq!(wants(&mut harness), ["read 7h35uxf5uhmm1 Some(1)"]);
        harness.state_mut().0.loading = false;
        harness.get_by_label("42 (SCOTT, active)").click();
        harness.run_steps(2);
        assert_eq!(wants(&mut harness), ["session 42,7"]);
        harness.get_by_label("Open in SQL window").click();
        harness.run_steps(2);
        assert_eq!(wants(&mut harness), ["open select 1 from dual"]);
    }
}
