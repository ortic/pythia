//! Execution plan as a collapsible tree with cost bars, and with the row
//! source statistics of the last execution next to the estimates when the
//! plan comes from the cursor cache (`pythia_db::cursor_plan`).

use std::collections::HashSet;

use egui::{Align, Layout, RichText, Sense, Ui};
use egui_extras::{Column as TableColumn, TableBuilder};
use pythia_db::{Cell, Column};

/// Estimates off by this factor or more are marked.
pub const MISESTIMATE: f64 = 10.0;

/// A misestimate is worth a flag outside the plan (the debugger's gutter)
/// only with this many rows on one side: estimated 1, got 12 is 12× but
/// costs nothing.
pub const NOTICEABLE_ROWS: f64 = 200.0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanRow {
    pub id: u32,
    pub parent: Option<u32>,
    pub depth: usize,
    pub operation: String,
    pub object: String,
    pub cost: Option<f64>,
    /// Estimated rows per start of the operation (E-Rows).
    pub rows: Option<f64>,
    pub bytes: Option<f64>,
    pub time: Option<f64>,
    pub access: Option<String>,
    pub filter: Option<String>,
    /// How often the operation ran in the last execution.
    pub starts: Option<f64>,
    /// Rows it returned over all its starts (A-Rows).
    pub actual_rows: Option<f64>,
    /// Seconds it took, its children's time included (A-Time).
    pub actual_time: Option<f64>,
    /// Logical reads (consistent and current gets).
    pub buffers: Option<f64>,
    pub reads: Option<f64>,
}

impl PlanRow {
    /// How far the estimate was off, as a factor of at least 1 either
    /// way, if the operation ran. E-Rows is per start and A-Rows the total
    /// over all starts, so A-Rows is held against E-Rows × Starts: under a
    /// nested loop comparing them directly is wrong.
    pub fn misestimate(&self) -> Option<f64> {
        let (estimate, actual, starts) = (self.rows?, self.actual_rows?, self.starts?);
        if starts <= 0.0 {
            return None;
        }
        let expected = (estimate * starts).max(1.0);
        let actual = actual.max(1.0);
        Some((actual / expected).max(expected / actual))
    }

    /// E-Rows × Starts against A-Rows, e.g. "estimated 20 rows, got
    /// 2,000 (100× more)" or "estimated 1 row per start × 318 starts =
    /// 318 rows, got none".
    pub fn misestimate_text(&self) -> Option<String> {
        let factor = self.misestimate()?;
        let (estimate, actual, starts) = (self.rows?, self.actual_rows?, self.starts?);
        let expected = estimate * starts;
        let rows = |n: f64| {
            let count = fmt_count(n);
            if count == "1" {
                "1 row".to_owned()
            } else {
                format!("{count} rows")
            }
        };
        // The optimizer estimates per start; the starts are the actual ones.
        let estimated = if starts > 1.0 {
            format!(
                "{} per start × {} starts = {}",
                rows(estimate),
                fmt_count(starts),
                rows(expected)
            )
        } else {
            rows(expected)
        };
        // The factor counts no rows as 1: "got 0 (318× fewer)" isn't true.
        let got = if fmt_count(actual) == "0" {
            "none".to_owned()
        } else {
            let direction = if actual >= expected { "more" } else { "fewer" };
            format!("{} ({}× {direction})", fmt_count(actual), fmt_count(factor))
        };
        Some(format!("estimated {estimated}, got {got}"))
    }
}

/// Whether the plan has row source statistics.
pub fn has_actuals(plan: &[PlanRow]) -> bool {
    plan.iter().any(|r| r.actual_rows.is_some())
}

/// The misestimate to look at first: the deepest one off by
/// [`MISESTIMATE`] or more (wrong estimates spread upwards, so the deepest
/// is usually where it starts), the larger one of equally deep ones.
pub fn worst_misestimate(plan: &[PlanRow]) -> Option<&PlanRow> {
    plan.iter()
        .filter_map(|r| {
            r.misestimate()
                .filter(|f| *f >= MISESTIMATE)
                .map(|f| (r, f))
        })
        .max_by(|(a, fa), (b, fb)| a.depth.cmp(&b.depth).then(fa.total_cmp(fb)))
        .map(|(r, _)| r)
}

/// The misestimate worth a flag outside the plan: the deepest off by
/// [`MISESTIMATE`] or more with [`NOTICEABLE_ROWS`] on one side.
pub fn flagged_misestimate(plan: &[PlanRow]) -> Option<&PlanRow> {
    let noticeable = |r: &PlanRow| {
        let expected = r.rows.unwrap_or(0.0) * r.starts.unwrap_or(0.0);
        expected.max(r.actual_rows.unwrap_or(0.0)) >= NOTICEABLE_ROWS
    };
    plan.iter()
        .filter_map(|r| {
            r.misestimate()
                .filter(|f| *f >= MISESTIMATE && noticeable(r))
                .map(|f| (r, f))
        })
        .max_by(|(a, fa), (b, fb)| a.depth.cmp(&b.depth).then(fa.total_cmp(fb)))
        .map(|(r, _)| r)
}

/// The worst misestimate as a note above a plan, with its line's id.
pub fn misestimate_note(plan: &[PlanRow]) -> Option<(u32, String)> {
    worst_misestimate(plan).map(|r| (r.id, note(r)))
}

/// The note on a misestimated plan line, e.g. "⚠ Row estimate off at plan
/// line 3 (TABLE ACCESS FULL PYTHIA.T): estimated 25,000 rows, got 200
/// (125× fewer)". The line is its Id, as DBMS_XPLAN shows it.
pub fn note(row: &PlanRow) -> String {
    let object = if row.object.is_empty() {
        String::new()
    } else {
        format!(" {}", row.object)
    };
    format!(
        "⚠ Row estimate off at plan line {} ({}{object}): {}",
        row.id,
        row.operation,
        row.misestimate_text().unwrap_or_default()
    )
}

/// Plan rows of a cursor from `pythia_db::cursor_plan`. Its statistics
/// are written when an execution ends: while the first one runs every
/// line has 0 starts, and the rows get none rather than zeros.
pub fn cursor_rows(columns: &[Column], data: &[Vec<Cell>]) -> Vec<PlanRow> {
    let mut rows = rows(columns, data);
    if !rows.iter().any(|r| r.starts.is_some_and(|s| s > 0.0)) {
        for r in &mut rows {
            r.starts = None;
            r.actual_rows = None;
            r.actual_time = None;
            r.buffers = None;
            r.reads = None;
        }
    }
    rows
}

/// Reads plan rows: PLAN_TABLE's (as returned by `MetaQuery::ExplainPlan`)
/// or the cursor cache's with statistics (`pythia_db::cursor_plan`).
pub fn rows(columns: &[Column], data: &[Vec<Cell>]) -> Vec<PlanRow> {
    let idx = |name: &str| {
        columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    };
    let (id, parent, depth, op, obj, cost, rows, bytes, time, access, filter) = (
        idx("ID"),
        idx("PARENT_ID"),
        idx("DEPTH"),
        idx("OPERATION"),
        idx("OBJECT"),
        idx("COST"),
        idx("ROWS"),
        idx("BYTES"),
        idx("TIME"),
        idx("ACCESS_PRED"),
        idx("FILTER_PRED"),
    );
    let (starts, a_rows, a_time, buffers, reads) = (
        idx("STARTS"),
        idx("A_ROWS"),
        idx("A_TIME"),
        idx("BUFFERS"),
        idx("READS"),
    );
    let text = |row: &Vec<Cell>, i: Option<usize>| i.and_then(|i| row.get(i)).cloned().flatten();
    let num = |row: &Vec<Cell>, i: Option<usize>| text(row, i).and_then(|v| parse_num(&v));
    data.iter()
        .map(|r| PlanRow {
            id: num(r, id).unwrap_or(0.0) as u32,
            parent: num(r, parent).map(|p| p as u32),
            depth: num(r, depth).unwrap_or(0.0) as usize,
            operation: text(r, op).unwrap_or_default(),
            object: text(r, obj).unwrap_or_default(),
            cost: num(r, cost),
            rows: num(r, rows),
            bytes: num(r, bytes),
            time: num(r, time),
            access: text(r, access),
            filter: text(r, filter),
            starts: num(r, starts),
            actual_rows: num(r, a_rows),
            actual_time: num(r, a_time),
            buffers: num(r, buffers),
            reads: num(r, reads),
        })
        .collect()
}

/// Numbers as Oracle returns them as text: `.5` for 0.5, and with a
/// decimal comma under some NLS settings.
fn parse_num(text: &str) -> Option<f64> {
    let text = text.trim().replace(',', ".");
    let text = match text.strip_prefix('.') {
        Some(rest) => format!("0.{rest}"),
        None => match text.strip_prefix("-.") {
            Some(rest) => format!("-0.{rest}"),
            None => text,
        },
    };
    text.parse().ok()
}

/// A plan as rows of text with predicates, for copying and the assistant.
/// With statistics it has DBMS_XPLAN's ALLSTATS columns.
pub fn text(plan: &[PlanRow]) -> String {
    let num = |v: Option<f64>| v.map(|v| format!("{v:.0}")).unwrap_or_default();
    let actuals = has_actuals(plan);
    let mut out = String::from(if actuals {
        "Id | Operation | Object | Starts | E-Rows | A-Rows | A-Time | Buffers | Reads | Cost"
    } else {
        "Id | Operation | Object | Rows | Bytes | Cost"
    });
    for r in plan {
        let operation = format!("{}{}", "  ".repeat(r.depth), r.operation);
        if actuals {
            let time = r.actual_time.map(fmt_seconds).unwrap_or_default();
            out.push_str(&format!(
                "\n{} | {operation} | {} | {} | {} | {} | {time} | {} | {} | {}",
                r.id,
                r.object,
                num(r.starts),
                num(r.rows),
                num(r.actual_rows),
                num(r.buffers),
                num(r.reads),
                num(r.cost)
            ));
            if let Some(off) = r
                .misestimate_text()
                .filter(|_| r.misestimate().is_some_and(|f| f >= MISESTIMATE))
            {
                out.push_str(&format!("    <- {off}"));
            }
        } else {
            out.push_str(&format!(
                "\n{} | {operation} | {} | {} | {} | {}",
                r.id,
                r.object,
                num(r.rows),
                num(r.bytes),
                num(r.cost)
            ));
        }
        if let Some(a) = &r.access {
            out.push_str(&format!("\n    access: {a}"));
        }
        if let Some(f) = &r.filter {
            out.push_str(&format!("\n    filter: {f}"));
        }
    }
    out
}

#[derive(Default)]
pub struct PlanView {
    collapsed: HashSet<u32>,
    selected: Option<u32>,
    /// [`PlanView::report_ui`] shows DBMS_XPLAN's text instead of the tree.
    show_text: bool,
}

impl PlanView {
    /// [`PlanView::report_ui`] shows DBMS_XPLAN's text rather than the tree.
    pub fn shows_text(&self) -> bool {
        self.show_text
    }

    /// A plan from the cursor cache: the tree, or DBMS_XPLAN's report of
    /// it (`text`) a click away, with the worst misestimate named and Copy
    /// (of the report). The tree fills the rest of `ui`.
    pub fn report_ui(&mut self, ui: &mut Ui, plan: &[PlanRow], text: &str, id: egui::Id) {
        if !plan.is_empty() {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.show_text, false, "Plan");
                ui.selectable_value(&mut self.show_text, true, "DBMS_XPLAN")
                    .on_hover_text("The plan as DBMS_XPLAN.DISPLAY_CURSOR shows it");
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(text.to_owned());
                }
                if let Some((line, note)) = misestimate_note(plan) {
                    let note =
                        egui::Label::new(RichText::new(note).color(ui.visuals().warn_fg_color))
                            .truncate()
                            .sense(Sense::click());
                    if ui.add(note).on_hover_text("Show the line").clicked() {
                        self.show_text = false;
                        self.selected = Some(line);
                    }
                }
            });
            if !self.show_text {
                self.ui(ui, plan, id);
                return;
            }
        }
        egui::ScrollArea::both()
            .id_salt(id.with("text"))
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).monospace()).extend());
            });
    }

    /// Selects a plan line (shows its predicates), e.g. the worst
    /// misestimate.
    pub fn select(&mut self, id: Option<u32>) {
        self.selected = id;
    }

    pub fn ui(&mut self, ui: &mut Ui, plan: &[PlanRow], id: egui::Id) {
        if plan.is_empty() {
            ui.weak("No plan");
            return;
        }
        let actuals = has_actuals(plan);
        let max_cost = plan
            .iter()
            .filter_map(|r| r.cost)
            .fold(0.0_f64, f64::max)
            .max(1.0);
        // Rows hidden below a collapsed ancestor.
        let mut hidden_below: Option<usize> = None;
        let visible: Vec<&PlanRow> = plan
            .iter()
            .filter(|r| {
                if let Some(d) = hidden_below {
                    if r.depth > d {
                        return false;
                    }
                    hidden_below = None;
                }
                if self.collapsed.contains(&r.id) {
                    hidden_below = Some(r.depth);
                }
                true
            })
            .collect();
        let has_children = |id: u32| plan.iter().any(|r| r.parent == Some(id));
        let accent = ui.visuals().hyperlink_color;
        let warn = ui.visuals().warn_fg_color;
        let row_height = crate::row_height(ui) + 2.0;
        let selected_bg = ui.visuals().selection.bg_fill;
        let mut toggle = None;
        let mut select = None;

        // The selected line's predicates below the table, each in a rect of
        // its own (tables don't keep clear of panels).
        let area = ui.available_rect_before_wrap();
        let details_height = if self.selected.is_some() {
            90.0_f32.min(area.height() / 2.0)
        } else {
            0.0
        };
        let (table_rect, details_rect) = area.split_top_bottom_at_y(area.max.y - details_height);
        if self.selected.is_some() {
            let mut details = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(details_rect.shrink2(egui::vec2(0.0, 2.0)))
                    .layout(Layout::top_down(Align::Min)),
            );
            details.painter().hline(
                details_rect.x_range(),
                details_rect.min.y,
                ui.visuals().widgets.noninteractive.bg_stroke,
            );
            let ui = &mut details;
            {
                if let Some(row) = self.selected.and_then(|s| plan.iter().find(|r| r.id == s)) {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("Line {}: {}", row.id, row.operation)).strong(),
                        );
                        if let Some(off) = row.misestimate_text() {
                            ui.label(RichText::new(off).color(
                                if row.misestimate().is_some_and(|f| f >= MISESTIMATE) {
                                    warn
                                } else {
                                    ui.visuals().text_color()
                                },
                            ));
                        }
                        if let Some(a) = &row.access {
                            ui.label(RichText::new(format!("access: {a}")).monospace());
                        }
                        if let Some(f) = &row.filter {
                            ui.label(RichText::new(format!("filter: {f}")).monospace());
                        }
                        if row.access.is_none() && row.filter.is_none() {
                            ui.weak("No predicates");
                        }
                    });
                }
            }
        }
        let mut table_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(table_rect)
                .layout(Layout::top_down(Align::Min)),
        );
        let parent = ui;
        let ui = &mut table_ui;

        // (title, right-aligned, hover text)
        let headers: &[(&str, bool, &str)] = if actuals {
            &[
                ("Operation", false, ""),
                ("Object", false, ""),
                ("Starts", true, "How often the operation ran"),
                ("E-Rows", true, "Rows estimated per start"),
                (
                    "A-Rows",
                    true,
                    "Rows returned over all starts; marked where E-Rows × Starts is off by 10× or more",
                ),
                ("A-Time", true, "Time taken, the operations below included"),
                ("Buffers", true, "Logical reads"),
                ("Reads", true, "Physical reads"),
                ("Cost", false, ""),
            ]
        } else {
            &[
                ("Operation", false, ""),
                ("Object", false, ""),
                ("Rows", true, ""),
                ("Bytes", true, ""),
                ("Cost", false, ""),
                ("Time", true, ""),
            ]
        };
        // Narrow, the statistics leave out physical reads and the cost.
        let compact = actuals && ui.available_width() < 900.0;
        let headers: Vec<&(&str, bool, &str)> = headers
            .iter()
            .filter(|(title, ..)| !compact || !matches!(*title, "Reads" | "Cost"))
            .collect();
        // The figures keep their width; operation and object share the rest.
        let (numbers, number_width, cost_width) = match (actuals, compact) {
            (true, true) => (5, 62.0, 0.0),
            (true, false) => (6, 62.0, 100.0),
            (false, _) => (2, 80.0, 150.0),
        };
        // Columns are apart by the item spacing, and a scroll bar may show.
        let columns = 2 + numbers + usize::from(!compact) + usize::from(!actuals);
        let gaps = columns as f32 * ui.spacing().item_spacing.x * 2.0 + 16.0;
        let rest =
            (ui.available_width() - numbers as f32 * number_width - cost_width - gaps).max(220.0);
        let mut table = TableBuilder::new(ui)
            .id_salt(id.with(actuals))
            .striped(true)
            .resizable(true)
            .auto_shrink(false)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(
                TableColumn::initial((rest * 0.7).min(380.0))
                    .at_least(140.0)
                    .clip(true),
            )
            .column(
                TableColumn::initial((rest * 0.3).min(180.0))
                    .at_least(60.0)
                    .clip(true),
            );
        for _ in 0..numbers {
            table = table.column(TableColumn::initial(number_width).clip(true));
        }
        if !compact {
            table = table.column(TableColumn::initial(cost_width).clip(true));
        }
        if !actuals {
            table = table.column(TableColumn::remainder());
        }
        table
            .header(row_height, |mut h| {
                for (title, right, hover) in headers {
                    h.col(|ui| {
                        let label = if *right {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.strong(*title)
                            })
                            .inner
                        } else {
                            ui.strong(*title)
                        };
                        if !hover.is_empty() {
                            label.on_hover_text(*hover);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, visible.len(), |mut row| {
                    let r = visible[row.index()];
                    let selected = self.selected == Some(r.id);
                    let (_, resp) = row.col(|ui| {
                        if selected {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, selected_bg);
                        }
                        ui.add_space(r.depth as f32 * 16.0);
                        if has_children(r.id) {
                            let open = !self.collapsed.contains(&r.id);
                            if ui
                                .add(
                                    egui::Button::new(if open { "⏷" } else { "⏵" })
                                        .frame(false)
                                        .small(),
                                )
                                .clicked()
                            {
                                toggle = Some(r.id);
                            }
                        } else {
                            ui.add_space(14.0);
                        }
                        let mut text = RichText::new(&r.operation).monospace();
                        if r.operation.contains("FULL") {
                            // Full scans are where tuning usually starts.
                            text = text.color(warn);
                        }
                        ui.label(text);
                        if r.access.is_some() || r.filter.is_some() {
                            ui.weak("*");
                        }
                    });
                    if resp.clicked() {
                        select = Some(r.id);
                    }
                    row.col(|ui| {
                        ui.label(RichText::new(&r.object).monospace());
                    });
                    if actuals {
                        row.col(|ui| right_num(ui, r.starts));
                        row.col(|ui| right_num(ui, r.rows));
                        row.col(|ui| {
                            let Some(actual) = r.actual_rows else {
                                return;
                            };
                            let off = r.misestimate().filter(|f| *f >= MISESTIMATE);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let mut text = RichText::new(fmt_num(actual)).monospace();
                                if off.is_some() {
                                    text = text.color(warn).strong();
                                }
                                let label = ui.label(text);
                                if off.is_some() {
                                    ui.label(RichText::new("⚠").color(warn));
                                }
                                if let Some(why) = r.misestimate_text() {
                                    label.on_hover_text(why);
                                }
                            });
                        });
                        row.col(|ui| {
                            if let Some(t) = r.actual_time {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(RichText::new(fmt_seconds(t)).monospace());
                                });
                            }
                        });
                        row.col(|ui| right_num(ui, r.buffers));
                        if !compact {
                            row.col(|ui| right_num(ui, r.reads));
                        }
                    } else {
                        row.col(|ui| right_num(ui, r.rows));
                        row.col(|ui| right_bytes(ui, r.bytes));
                    }
                    if !compact {
                        row.col(|ui| {
                            if let Some(cost) = r.cost {
                                let width = if actuals { 40.0 } else { 80.0 };
                                let (rect, _) =
                                    ui.allocate_exact_size(egui::vec2(width, 8.0), Sense::hover());
                                ui.painter().rect_filled(
                                    rect,
                                    3.0,
                                    ui.visuals().faint_bg_color.gamma_multiply(2.0),
                                );
                                let filled = egui::Rect::from_min_size(
                                    rect.min,
                                    egui::vec2(width * (cost / max_cost) as f32, rect.height()),
                                );
                                ui.painter().rect_filled(filled, 3.0, accent);
                                ui.label(RichText::new(fmt_num(cost)).monospace());
                            }
                        });
                    }
                    if !actuals {
                        row.col(|ui| {
                            if let Some(t) = r.time {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(RichText::new(fmt_time(t)).monospace());
                                });
                            }
                        });
                    }
                });
            });
        // The area, and what the table needs beyond it: a resizable panel
        // the plan is in grows to that.
        parent.allocate_rect(area.union(ui.min_rect()), Sense::hover());
        if let Some(id) = toggle
            && !self.collapsed.remove(&id)
        {
            self.collapsed.insert(id);
        }
        if let Some(id) = select {
            self.selected = if self.selected == Some(id) {
                None
            } else {
                Some(id)
            };
        }
    }
}

fn right_num(ui: &mut Ui, v: Option<f64>) {
    if let Some(v) = v {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(fmt_num(v)).monospace());
        });
    }
}

fn right_bytes(ui: &mut Ui, v: Option<f64>) {
    if let Some(v) = v {
        let text = match v {
            v if v >= 1e9 => format!("{:.1}G", v / 1e9),
            v if v >= 1e6 => format!("{:.1}M", v / 1e6),
            v if v >= 1e3 => format!("{:.1}K", v / 1e3),
            v => format!("{v:.0}"),
        };
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(text).monospace());
        });
    }
}

/// Large numbers with K/M suffixes.
pub fn fmt_num(v: f64) -> String {
    match v {
        v if v >= 1e9 => format!("{:.1}G", v / 1e9),
        v if v >= 1e6 => format!("{:.1}M", v / 1e6),
        v if v >= 1e4 => format!("{:.0}K", v / 1e3),
        v => format!("{v:.0}"),
    }
}

/// A count with thousands separators, e.g. 12,345.
fn fmt_count(v: f64) -> String {
    let digits = format!("{:.0}", v.abs());
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if v < 0.0 { format!("-{out}") } else { out }
}

/// A measured time: milliseconds below a second.
pub fn fmt_seconds(seconds: f64) -> String {
    match seconds {
        s if s < 0.01 => format!("{:.2} ms", s * 1000.0),
        s if s < 1.0 => format!("{:.0} ms", s * 1000.0),
        s if s < 60.0 => format!("{s:.2} s"),
        s => fmt_time(s),
    }
}

/// PLAN_TABLE.TIME is in seconds.
fn fmt_time(seconds: f64) -> String {
    let s = seconds as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: u32, depth: usize, rows: f64, starts: f64, actual: f64) -> PlanRow {
        PlanRow {
            id,
            depth,
            operation: format!("OP {id}"),
            rows: Some(rows),
            starts: Some(starts),
            actual_rows: Some(actual),
            ..Default::default()
        }
    }

    #[test]
    fn estimates_are_per_start() {
        // A nested loop's inner index lookup: 1 row per start, 500 starts.
        let inner = line(3, 2, 1.0, 500.0, 500.0);
        assert_eq!(inner.misestimate(), Some(1.0));
        let off = line(2, 1, 20.0, 1.0, 2000.0);
        assert_eq!(off.misestimate(), Some(100.0));
        assert_eq!(
            off.misestimate_text().as_deref(),
            Some("estimated 20 rows, got 2,000 (100× more)")
        );
        assert_eq!(
            line(6, 1, 1.0, 1.0, 3000.0).misestimate_text().as_deref(),
            Some("estimated 1 row, got 3,000 (3,000× more)")
        );
        let fewer = line(4, 1, 30.0, 10.0, 3.0);
        assert_eq!(fewer.misestimate(), Some(100.0));
        assert_eq!(
            fewer.misestimate_text().as_deref(),
            Some("estimated 30 rows per start × 10 starts = 300 rows, got 3 (100× fewer)")
        );
        // A unique scan that never found its row: 0 isn't 318× fewer.
        let none = PlanRow {
            object: "IFSAPP.X_PK".into(),
            operation: "INDEX UNIQUE SCAN".into(),
            ..line(35, 3, 1.0, 318.0, 0.0)
        };
        assert_eq!(none.misestimate(), Some(318.0));
        assert_eq!(
            note(&none),
            "⚠ Row estimate off at plan line 35 (INDEX UNIQUE SCAN IFSAPP.X_PK): \
             estimated 1 row per start × 318 starts = 318 rows, got none"
        );
        // Never started (the other branch of a filter): nothing to hold
        // against the estimate. Without statistics neither.
        assert_eq!(line(5, 1, 10.0, 0.0, 0.0).misestimate(), None);
        let estimate_only = PlanRow {
            rows: Some(5.0),
            ..Default::default()
        };
        assert_eq!(estimate_only.misestimate(), None);
    }

    #[test]
    fn flags_need_rows() {
        // 12× off, but 1 row against 12: not worth a flag outside the plan.
        let small = vec![line(0, 0, 1.0, 1.0, 12.0)];
        assert!(worst_misestimate(&small).is_some());
        assert!(flagged_misestimate(&small).is_none());
        let big = vec![line(0, 0, 25_000.0, 1.0, 200.0), line(1, 1, 1.0, 1.0, 12.0)];
        assert_eq!(flagged_misestimate(&big).map(|r| r.id), Some(0));
    }

    #[test]
    fn the_deepest_misestimate_comes_first() {
        let plan = vec![
            line(0, 0, 1.0, 1.0, 1.0),
            line(1, 1, 5.0, 1.0, 50_000.0),
            line(2, 2, 20.0, 1.0, 2000.0),
            line(3, 3, 100.0, 1.0, 120.0),
        ];
        assert_eq!(worst_misestimate(&plan).map(|r| r.id), Some(2));
        assert!(worst_misestimate(&plan[3..]).is_none());
    }

    #[test]
    fn reads_statistics_columns() {
        let columns: Vec<Column> = [
            "ID",
            "PARENT_ID",
            "DEPTH",
            "OPERATION",
            "OBJECT",
            "COST",
            "ROWS",
            "BYTES",
            "TIME",
            "ACCESS_PRED",
            "FILTER_PRED",
            "STARTS",
            "A_ROWS",
            "A_TIME",
            "BUFFERS",
            "READS",
        ]
        .into_iter()
        .map(|name| Column {
            name: name.into(),
            type_name: String::new(),
            numeric: false,
        })
        .collect();
        let cell = |v: &str| (!v.is_empty()).then(|| v.to_owned());
        let data = [
            [
                "0",
                "",
                "0",
                "SELECT STATEMENT",
                "",
                "3",
                "",
                "",
                "",
                "",
                "",
                "1",
                "1",
                ".000412",
                "7",
                "0",
            ],
            [
                "1",
                "0",
                "1",
                "TABLE ACCESS FULL",
                "PYTHIA.T",
                "3",
                "20",
                "80",
                "1",
                "",
                "UPPER(TO_CHAR(\"N\"))=TO_CHAR(\"N\")",
                "1",
                "2000",
                ",0004",
                "7",
                "0",
            ],
        ]
        .iter()
        .map(|r| r.iter().map(|v| cell(v)).collect())
        .collect::<Vec<Vec<Cell>>>();
        let plan = rows(&columns, &data);
        assert!(has_actuals(&plan));
        assert_eq!(plan[0].actual_time, Some(0.000412));
        assert_eq!(plan[1].actual_time, Some(0.0004));
        assert_eq!(plan[1].misestimate(), Some(100.0));
        let out = text(&plan);
        assert!(
            out.starts_with("Id | Operation | Object | Starts | E-Rows | A-Rows"),
            "{out}"
        );
        assert!(
            out.contains("1 |   TABLE ACCESS FULL | PYTHIA.T | 1 | 20 | 2000 | 0.40 ms | 7 | 0 | 3    <- estimated 20 rows, got 2,000 (100× more)"),
            "{out}"
        );
        // EXPLAIN PLAN's rows have no statistics.
        let explain = rows(&columns[..11], &[data[1][..11].to_vec()]);
        assert!(!has_actuals(&explain));
        assert!(text(&explain).starts_with("Id | Operation | Object | Rows | Bytes | Cost"));
    }
}
