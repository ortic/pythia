//! The debugger's SQL panel: the statements the stopped session ran, each
//! with the line it came from, and a statement's plan with estimated and
//! actual rows side by side. At every stop it selects the statement at the
//! stopped line, or the closest one before it in the unit.

use egui::{Align, Layout, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use pythia_ui::plan::{self, PlanView};

use crate::sql::{CursorPlan, SessionSql, TargetSession};
use crate::types::{Location, Unit};
use crate::ui::{DebugAction, SqlQuestion};

pub(crate) struct SqlPanel {
    /// The panel is shown (kept across restarts).
    pub show: bool,
    /// The debugged session collects row source statistics (kept).
    pub statistics: bool,
    /// Stops so far: an answer for an earlier stop is outdated.
    stop: u64,
    /// The stop a lookup is on its way for.
    finding: Option<u64>,
    /// The statements found at a stop, or why there are none.
    found: Option<(u64, Result<Vec<SessionSql>, String>)>,
    /// The statement shown: SQL_ID and child.
    selected: Option<(String, i64)>,
    /// The user picked `selected` at this stop; else it follows the code.
    picked: bool,
    /// The plan being loaded.
    loading: Option<(String, i64)>,
    /// The selected statement's plan, or why it isn't there.
    plan: Option<((String, i64), Result<CursorPlan, String>)>,
    view: PlanView,
}

impl Default for SqlPanel {
    fn default() -> Self {
        Self {
            show: true,
            statistics: true,
            stop: 0,
            finding: None,
            found: None,
            selected: None,
            picked: false,
            loading: None,
            plan: None,
            view: PlanView::default(),
        }
    }
}

/// Where the panel's statements are: the stop and the session.
pub(crate) struct Context<'a> {
    pub suspended: bool,
    pub active: bool,
    /// The selected frame's unit and line.
    pub at: Option<(&'a Unit, u32)>,
    /// The anonymous block being debugged (statements with no unit are
    /// its).
    pub block: Option<&'a Unit>,
    /// The selected frame's variables as `name = value`, for the assistant.
    pub variables: Vec<String>,
}

impl SqlPanel {
    /// A new debug run: nothing found yet.
    pub fn reset(&mut self) {
        let (show, statistics) = (self.show, self.statistics);
        *self = Self {
            show,
            statistics,
            ..Self::default()
        };
    }

    /// Execution stopped: look the statements up again.
    pub fn stopped(&mut self, session: Option<&TargetSession>, out: &mut Vec<DebugAction>) {
        self.stop += 1;
        self.picked = false;
        self.find(session, out);
    }

    /// Asks for the statements at this stop, unless a lookup is on its way
    /// (its answer asks again if it is outdated by then).
    pub fn find(&mut self, session: Option<&TargetSession>, out: &mut Vec<DebugAction>) {
        if !self.show || self.finding.is_some() {
            return;
        }
        let Some(session) = session else {
            return;
        };
        if self
            .found
            .as_ref()
            .is_some_and(|(stop, _)| *stop == self.stop)
        {
            return;
        }
        self.finding = Some(self.stop);
        out.push(DebugAction::FindSql {
            session: session.clone(),
            stop: self.stop,
        });
    }

    /// The statements of stop `stop`. Selects the one at the code (unless
    /// the user picked one) and loads its plan.
    pub fn found(
        &mut self,
        stop: u64,
        result: Result<Vec<SessionSql>, String>,
        session: Option<&TargetSession>,
        cx: &Context,
        out: &mut Vec<DebugAction>,
    ) {
        self.finding = None;
        if stop != self.stop {
            self.find(session, out);
            return;
        }
        self.found = Some((stop, result));
        if !self.picked {
            self.follow(cx);
        }
        self.load(out);
    }

    /// Another frame was selected: show its statement.
    pub fn frame_selected(&mut self, cx: &Context, out: &mut Vec<DebugAction>) {
        if !self.picked {
            self.follow(cx);
            self.load(out);
        }
    }

    pub fn plan_loaded(&mut self, sql_id: String, child: i64, result: Result<CursorPlan, String>) {
        let key = (sql_id, child);
        if self.loading.as_ref() == Some(&key) {
            self.loading = None;
        }
        if self.selected.as_ref() != Some(&key) {
            return;
        }
        self.plan = Some((key, result));
    }

    /// The plan shown, once loaded.
    pub fn shown_plan(&self) -> Option<&CursorPlan> {
        match &self.plan {
            Some((key, Ok(plan))) if self.selected.as_ref() == Some(key) => Some(plan),
            _ => None,
        }
    }

    pub fn statements(&self) -> &[SessionSql] {
        match &self.found {
            Some((_, Ok(list))) => list,
            _ => &[],
        }
    }

    /// Selects the statement at the stopped line, else the closest one
    /// before it in the unit, else the first listed (a running one, or the
    /// most recent).
    fn follow(&mut self, cx: &Context) {
        let list = self.statements();
        let in_unit = |s: &SessionSql, unit: &Unit| match &s.unit {
            Some(u) => u == unit,
            None => cx.block == Some(unit),
        };
        let pick = cx
            .at
            .and_then(|(unit, line)| {
                list.iter()
                    .filter(|s| in_unit(s, unit) && s.line <= line)
                    .max_by_key(|s| s.line)
            })
            .or_else(|| list.first());
        self.selected = pick.map(|s| (s.sql_id.clone(), s.child));
    }

    fn load(&mut self, out: &mut Vec<DebugAction>) {
        let Some((sql_id, child)) = self.selected.clone() else {
            self.plan = None;
            return;
        };
        if self.loading.as_ref() == Some(&(sql_id.clone(), child)) {
            return;
        }
        self.loading = Some((sql_id.clone(), child));
        out.push(DebugAction::LoadPlan { sql_id, child });
    }

    fn label(s: &SessionSql) -> String {
        match &s.unit {
            Some(unit) => format!("{}:{}", unit.name, s.line),
            None => format!("block:{}", s.line),
        }
    }

    fn location(s: &SessionSql, block: Option<&Unit>) -> Option<Location> {
        let unit = s.unit.clone().or_else(|| block.cloned())?;
        Some(Location {
            unit,
            line: Some(s.line),
        })
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &Context, out: &mut Vec<DebugAction>) {
        let count = self.statements().len();
        ui.horizontal(|ui| {
            ui.strong("SQL").on_hover_text(
                "Statements the debugged session ran from PL/SQL since the start, by line, \
                 and the selected one's plan with estimated and actual rows",
            );
            if count > 0 {
                ui.weak(format!(
                    "{count} statement{}",
                    if count == 1 { "" } else { "s" }
                ));
            }
            if self.finding.is_some() || self.loading.is_some() {
                ui.spinner();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.plan_buttons(ui, cx, out);
            });
        });
        let (stop, result) = match &self.found {
            Some((stop, result)) => (*stop, result),
            None => {
                let text = if cx.active {
                    "Stop at a breakpoint or step: the statements the session ran show here, \
                     the one at the stopped line with its plan."
                } else {
                    "Debug code and stop in it: the statements it ran show here by line, each \
                     with its plan, estimated and actual rows side by side."
                };
                ui.add(egui::Label::new(RichText::new(text).weak()).wrap());
                return;
            }
        };
        // A copy: selecting below changes the panel.
        let list = match result {
            Ok(list) => list.clone(),
            Err(e) => {
                ui.add(egui::Label::new(RichText::new(e).color(ui.visuals().warn_fg_color)).wrap());
                return;
            }
        };
        if list.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new("The code hasn't run SQL since the start yet.").weak(),
                )
                .wrap(),
            );
            return;
        }
        let outdated = !cx.suspended || stop != self.stop;
        let current = cx.at.and_then(|(unit, line)| {
            list.iter().find(|s| {
                s.line == line
                    && match &s.unit {
                        Some(u) => u == unit,
                        None => cx.block == Some(unit),
                    }
            })
        });
        let current = current.map(|s| s.sql_id.clone());
        let mut select = None;
        let mut open = None;
        let row_height = pythia_ui::row_height(ui);
        let selected_bg = ui.visuals().selection.bg_fill;
        let list_height = (row_height * (list.len() as f32 + 1.0) + 4.0).min(row_height * 4.5);
        ui.allocate_ui(egui::vec2(ui.available_width(), list_height), |ui| {
            TableBuilder::new(ui)
                .id_salt("dbg-sql-list")
                .striped(true)
                .auto_shrink([false, true])
                .max_scroll_height(list_height)
                .sense(Sense::click())
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::initial(170.0).at_least(80.0).clip(true))
                .column(Column::remainder().at_least(120.0).clip(true))
                .column(Column::initial(50.0))
                .column(Column::initial(70.0))
                .header(row_height, |mut h| {
                    h.col(|ui| {
                        ui.strong("Line");
                    });
                    h.col(|ui| {
                        ui.strong("Statement");
                    });
                    h.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.strong("Runs")
                                .on_hover_text("Executions, by all sessions using the cursor");
                        });
                    });
                    h.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.strong("Time")
                                .on_hover_text("Elapsed time of those executions");
                        });
                    });
                })
                .body(|body| {
                    body.rows(row_height, list.len(), |mut row| {
                        let s = &list[row.index()];
                        let selected = self.selected.as_ref() == Some(&(s.sql_id.clone(), s.child));
                        let (_, resp) = row.col(|ui| {
                            if selected {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, selected_bg);
                            }
                            let marker = if current.as_ref() == Some(&s.sql_id) {
                                "▶ "
                            } else {
                                "   "
                            };
                            ui.label(RichText::new(marker).monospace());
                            let link = ui.link(RichText::new(Self::label(s)).monospace());
                            if link.clicked() {
                                open = Self::location(s, cx.block);
                            }
                            link.on_hover_text(match &s.unit {
                                Some(unit) => format!("Open {unit} at line {}", s.line),
                                None => format!("Line {} of the block being debugged", s.line),
                            });
                        });
                        if resp.clicked() {
                            select = Some((s.sql_id.clone(), s.child));
                        }
                        let (_, resp) = row.col(|ui| {
                            let mut text = RichText::new(
                                s.text.split_whitespace().collect::<Vec<_>>().join(" "),
                            )
                            .monospace();
                            if outdated {
                                text = text.weak();
                            }
                            ui.add(egui::Label::new(text).truncate().selectable(false))
                                .on_hover_text(RichText::new(&s.text).monospace());
                            if s.running {
                                ui.label(
                                    RichText::new("running")
                                        .small()
                                        .color(ui.visuals().warn_fg_color),
                                );
                            }
                        });
                        if resp.clicked() {
                            select = Some((s.sql_id.clone(), s.child));
                        }
                        row.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(s.executions.to_string()).monospace());
                            });
                        });
                        row.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(plan::fmt_seconds(s.elapsed)).monospace());
                            });
                        });
                    });
                });
        });
        if let Some(location) = open {
            out.push(DebugAction::OpenSource(location));
        }
        if let Some(key) = select
            && self.selected.as_ref() != Some(&key)
        {
            self.selected = Some(key);
            self.picked = true;
            self.load(out);
        }
        if outdated {
            ui.weak("As of the last stop");
        }
        ui.separator();
        self.plan_ui(ui, &list);
    }

    /// Ask AI and Copy plan for the plan shown (right to left).
    fn plan_buttons(&self, ui: &mut Ui, cx: &Context, out: &mut Vec<DebugAction>) {
        let Some(plan) = self.shown_plan() else {
            return;
        };
        if ui
            .button("Copy plan")
            .on_hover_text("DBMS_XPLAN's report of the plan")
            .clicked()
        {
            let text = plan
                .xplan
                .clone()
                .unwrap_or_else(|_| plan::text(&plan.rows));
            ui.ctx().copy_text(text);
        }
        let ask = ui.button("Ask AI").on_hover_text(
            "Analyze the statement with its plan, the statistics and the variables at the stop",
        );
        if ask.clicked() {
            let statement = self
                .statements()
                .iter()
                .find(|s| s.sql_id == plan.sql_id && s.child == plan.child);
            let notes = notes(plan, self.statistics);
            out.push(DebugAction::AnalyzeSql(question(
                plan, statement, cx, &notes,
            )));
        }
        ui.add(
            egui::Label::new(
                RichText::new(format!("{} · child {}", plan.sql_id, plan.child))
                    .monospace()
                    .weak(),
            )
            .truncate(),
        );
    }

    fn plan_ui(&mut self, ui: &mut Ui, list: &[SessionSql]) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        if !list
            .iter()
            .any(|s| (&s.sql_id, s.child) == (&selected.0, selected.1))
        {
            return;
        }
        let plan = match &self.plan {
            Some((key, plan)) if *key == selected => plan,
            _ => {
                ui.weak("Loading the plan…");
                return;
            }
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(e) => {
                ui.add(egui::Label::new(RichText::new(e).color(ui.visuals().warn_fg_color)).wrap());
                return;
            }
        };
        let warn = ui.visuals().warn_fg_color;
        let notes = notes(plan, self.statistics);
        let worst = plan::worst_misestimate(&plan.rows).map(|r| r.id);
        for (i, (note, is_warning)) in notes.iter().enumerate() {
            let text = RichText::new(note).small();
            let label = ui.add(
                egui::Label::new(if *is_warning {
                    text.color(warn)
                } else {
                    text.weak()
                })
                .wrap()
                .sense(Sense::click()),
            );
            // The first note is the misestimate, if there is one.
            if i == 0
                && let Some(id) = worst
                && label.on_hover_text("Show the line").clicked()
            {
                self.view.select(Some(id));
            }
        }
        self.view.ui(
            ui,
            &plan.rows,
            egui::Id::new(("dbg-sql-plan", &plan.sql_id, plan.child)),
        );
    }
}

/// What to know about a plan before reading it: (note, is a warning).
fn notes(plan: &CursorPlan, statistics: bool) -> Vec<(String, bool)> {
    let mut notes = Vec::new();
    let measured = plan.measured();
    if let Some(worst) = plan::worst_misestimate(&plan.rows) {
        notes.push((
            format!(
                "⚠ Estimate off at {} {}{}: {}",
                worst.id,
                worst.operation,
                if worst.object.is_empty() {
                    String::new()
                } else {
                    format!(" {}", worst.object)
                },
                worst.misestimate_text().unwrap_or_default()
            ),
            true,
        ));
    } else if measured {
        notes.push((
            "Estimates within 10× of the actual rows on every line".into(),
            false,
        ));
    }
    match (plan.running, measured) {
        (true, true) => notes.push((
            "Running now: the figures are of its last finished execution".into(),
            false,
        )),
        (true, false) => notes.push((
            "Running now for the first time: actual rows come when its cursor is closed or \
             fetched to the end (step past the loop)"
                .into(),
            false,
        )),
        (false, false) if statistics => notes.push((
            "No actual rows: the statement ran before the debug run, or its last execution \
             didn't collect statistics"
                .into(),
            false,
        )),
        (false, false) => notes.push((
            "Estimates only: turn on Plan statistics in the debugger toolbar and debug again \
             for actual rows"
                .into(),
            false,
        )),
        (false, true) => {}
    }
    if let Some((child, hash, executions)) = plan.other_plans.first() {
        notes.push((
            format!(
                "Other sessions may run it with another plan: {hash} (child {child}, {executions} \
                 runs){}",
                if plan.other_plans.len() > 1 {
                    format!(", and {} more", plan.other_plans.len() - 1)
                } else {
                    String::new()
                }
            ),
            true,
        ));
    }
    notes
}

/// What the assistant gets about a statement.
fn question(
    plan: &CursorPlan,
    statement: Option<&SessionSql>,
    cx: &Context,
    notes: &[(String, bool)],
) -> SqlQuestion {
    let location = statement.and_then(|s| SqlPanel::location(s, cx.block));
    let at = match (&location, statement) {
        (Some(l), Some(s)) => format!(" at {} line {}", l.unit, s.line),
        _ => String::new(),
    };
    let mut report = match &plan.xplan {
        Ok(xplan) => xplan.clone(),
        Err(_) => plan::text(&plan.rows),
    };
    for (note, _) in notes {
        report.push_str(&format!("\n{note}"));
    }
    SqlQuestion {
        title: format!("statement {}{at}", plan.sql_id),
        sql: plan.text.clone(),
        plan: report,
        measured: plan.measured(),
        variables: cx.variables.clone(),
        location,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UnitType;

    fn sql(id: &str, unit: Option<&Unit>, line: u32) -> SessionSql {
        SessionSql {
            sql_id: id.into(),
            child: 0,
            plan_hash: 1,
            unit: unit.cloned(),
            line,
            text: format!("SELECT {id} FROM DUAL"),
            executions: 1,
            elapsed: 0.001,
            buffer_gets: 3,
            running: false,
            last_active: "10:00:00".into(),
        }
    }

    fn cx<'a>(at: Option<(&'a Unit, u32)>, block: Option<&'a Unit>) -> Context<'a> {
        Context {
            suspended: true,
            active: true,
            at,
            block,
            variables: Vec::new(),
        }
    }

    #[test]
    fn follows_the_stopped_line() {
        let pkg = Unit::new("PYTHIA", "PKG", UnitType::PackageBody);
        let other = Unit::new("PYTHIA", "OTHER", UnitType::Procedure);
        let block = Unit::new("PYTHIA", "abc", UnitType::Block);
        let session = TargetSession {
            sid: 7,
            started: "2026-10-29 10:00:00".into(),
        };
        let mut panel = SqlPanel::default();
        let mut out = Vec::new();
        panel.stopped(Some(&session), &mut out);
        assert_eq!(
            out,
            vec![DebugAction::FindSql {
                session: session.clone(),
                stop: 1
            }]
        );
        // The last run first, as the lookup orders them.
        let list = vec![
            sql("other", Some(&other), 3),
            sql("at12", Some(&pkg), 12),
            sql("at20", Some(&pkg), 20),
            sql("block", None, 2),
        ];
        out.clear();
        panel.found(
            1,
            Ok(list.clone()),
            Some(&session),
            &cx(Some((&pkg, 15)), Some(&block)),
            &mut out,
        );
        // Line 15 has none: the closest before it in the unit.
        assert_eq!(panel.selected, Some(("at12".into(), 0)));
        assert_eq!(
            out,
            vec![DebugAction::LoadPlan {
                sql_id: "at12".into(),
                child: 0
            }]
        );

        // The next stop is in the block, at the statement's line.
        out.clear();
        panel.stopped(Some(&session), &mut out);
        panel.found(
            2,
            Ok(list.clone()),
            Some(&session),
            &cx(Some((&block, 2)), Some(&block)),
            &mut out,
        );
        assert_eq!(panel.selected, Some(("block".into(), 0)));

        // Before any of the unit's statements: the one run last.
        panel.stopped(Some(&session), &mut out);
        panel.found(
            3,
            Ok(list.clone()),
            Some(&session),
            &cx(Some((&pkg, 5)), Some(&block)),
            &mut out,
        );
        assert_eq!(panel.selected, Some(("other".into(), 0)));
    }

    #[test]
    fn outdated_answers_ask_again() {
        let session = TargetSession {
            sid: 7,
            started: "2026-10-29 10:00:00".into(),
        };
        let mut panel = SqlPanel::default();
        let mut out = Vec::new();
        panel.stopped(Some(&session), &mut out);
        // Stepping on before the answer: no second lookup meanwhile.
        panel.stopped(Some(&session), &mut out);
        assert_eq!(out.len(), 1);
        out.clear();
        panel.found(1, Ok(vec![]), Some(&session), &cx(None, None), &mut out);
        assert!(panel.found.is_none());
        assert_eq!(
            out,
            vec![DebugAction::FindSql {
                session: session.clone(),
                stop: 2
            }]
        );
        out.clear();
        panel.found(2, Ok(vec![]), Some(&session), &cx(None, None), &mut out);
        assert!(out.is_empty());
        assert!(panel.found.is_some());

        // Hidden, it looks nothing up.
        panel.show = false;
        panel.stopped(Some(&session), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn notes_say_what_the_figures_are() {
        let row = |id, depth, rows, starts, actual| pythia_ui::plan::PlanRow {
            id,
            depth,
            operation: "TABLE ACCESS FULL".into(),
            object: "PYTHIA.T".into(),
            rows: Some(rows),
            starts,
            actual_rows: starts.map(|_| actual),
            ..Default::default()
        };
        let mut plan = CursorPlan {
            sql_id: "x".into(),
            child: 1,
            rows: vec![
                row(0, 0, 1.0, Some(1.0), 1.0),
                row(1, 1, 20.0, Some(1.0), 2000.0),
            ],
            text: "SELECT".into(),
            executions: 2,
            running: true,
            other_plans: vec![(0, 42, 17)],
            xplan: Ok(String::new()),
        };
        let texts =
            |notes: Vec<(String, bool)>| notes.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(
            texts(notes(&plan, true)),
            [
                "⚠ Estimate off at 1 TABLE ACCESS FULL PYTHIA.T: estimated 20 rows, got 2,000 (100× more)",
                "Running now: the figures are of its last finished execution",
                "Other sessions may run it with another plan: 42 (child 0, 17 runs)",
            ]
        );
        plan.rows = vec![row(0, 0, 1.0, None, 0.0)];
        plan.running = false;
        plan.other_plans.clear();
        assert_eq!(
            texts(notes(&plan, false)),
            [
                "Estimates only: turn on Plan statistics in the debugger toolbar and debug again for actual rows"
            ]
        );
    }
}
