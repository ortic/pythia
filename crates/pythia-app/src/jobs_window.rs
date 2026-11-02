//! Scheduler jobs: DBMS_SCHEDULER jobs with their state, schedule and run
//! history; run, stop, enable, disable and drop them, and open the job form
//! for a new job or a job's settings.

use std::collections::BTreeMap;

use egui::{Color32, RichText, Ui};
use egui_extras::{Column as TableColumn, TableBuilder};
use pythia_db::oracle::Connection;
use pythia_db::{DbError, DbEvent, DbRequest};
use serde::{Deserialize, Serialize};

use crate::documents::{DocContext, Effects, OpenRequest, Target, quote_ident, request_id};

#[derive(Default, Serialize, Deserialize)]
pub struct JobsWindow {
    /// Include jobs of Oracle-maintained schemas.
    pub system: bool,
    pub filter: String,
    #[serde(skip)]
    state: State,
}

#[derive(Default)]
struct State {
    loaded: bool,
    jobs: Vec<Job>,
    selected: Option<(String, String)>,
    runs: Vec<JobRun>,
    pending: BTreeMap<u64, Pending>,
    error: Option<String>,
    notice: Option<String>,
    confirm_drop: bool,
}

enum Pending {
    Jobs,
    Runs,
    Action(&'static str),
}

#[derive(Debug, Clone, Default)]
pub struct Job {
    pub owner: String,
    pub name: String,
    pub enabled: bool,
    pub state: String,
    pub job_type: String,
    pub action: String,
    pub schedule: String,
    pub last_start: String,
    pub last_seconds: Option<f64>,
    pub next_run: String,
    pub runs: i64,
    pub failures: i64,
    pub comments: String,
}

#[derive(Debug, Clone, Default)]
pub struct JobRun {
    pub start: String,
    pub status: String,
    pub seconds: Option<f64>,
    pub error: Option<i64>,
    pub info: String,
}

/// Seconds of an INTERVAL DAY TO SECOND column, as SQL.
fn seconds(col: &str) -> String {
    format!(
        "extract(day from {col}) * 86400 + extract(hour from {col}) * 3600 \
         + extract(minute from {col}) * 60 + extract(second from {col})"
    )
}

pub fn load_jobs(conn: &Connection, system: bool) -> Result<Vec<Job>, DbError> {
    let owners = if system {
        String::new()
    } else {
        let oracle = pythia_db::compat::schemas_query(pythia_db::compat::release(conn)?, true);
        format!("where j.owner not in ({oracle})")
    };
    let sql = format!(
        "select j.owner, j.job_name, j.enabled, j.state, j.job_type,
                substr(coalesce(j.job_action, j.program_owner || '.' || j.program_name), 1, 4000),
                coalesce(j.repeat_interval, j.schedule_owner || '.' || j.schedule_name,
                         case j.schedule_type when 'ONCE' then 'once' else lower(j.schedule_type) end),
                to_char(j.last_start_date, 'YYYY-MM-DD HH24:MI:SS'),
                {},
                to_char(j.next_run_date, 'YYYY-MM-DD HH24:MI:SS'),
                j.run_count, j.failure_count, j.comments
           from all_scheduler_jobs j
          {owners}
          order by j.owner, j.job_name",
        seconds("j.last_run_duration")
    );
    let mut out = Vec::new();
    for row in conn.query(&sql, &[])? {
        let row = row?;
        let text = |i: usize| -> Result<String, DbError> {
            Ok(row.get::<_, Option<String>>(i)?.unwrap_or_default())
        };
        out.push(Job {
            owner: text(0)?,
            name: text(1)?,
            enabled: text(2)? == "TRUE",
            state: text(3)?,
            job_type: text(4)?,
            action: text(5)?.trim_matches('.').to_owned(),
            schedule: text(6)?.trim_matches('.').to_owned(),
            last_start: text(7)?,
            last_seconds: row.get(8)?,
            next_run: text(9)?,
            runs: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
            failures: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
            comments: text(12)?,
        });
    }
    Ok(out)
}

pub fn load_runs(conn: &Connection, owner: &str, name: &str) -> Result<Vec<JobRun>, DbError> {
    let sql = format!(
        "select to_char(actual_start_date, 'YYYY-MM-DD HH24:MI:SS'), status, {}, error#,
                substr(additional_info, 1, 2000)
           from all_scheduler_job_run_details
          where owner = :1 and job_name = :2
          order by actual_start_date desc nulls last, log_id desc",
        seconds("run_duration")
    );
    let sql = pythia_db::compat::first_rows(&sql, 50);
    let mut out = Vec::new();
    for row in conn.query(&sql, &[&owner, &name])? {
        let row = row?;
        out.push(JobRun {
            start: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            status: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            seconds: row.get(2)?,
            error: row.get::<_, Option<i64>>(3)?.filter(|e| *e != 0),
            info: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        });
    }
    Ok(out)
}

/// The PL/SQL for an action on a job.
pub fn action_sql(action: &str, owner: &str, name: &str) -> String {
    let job = format!(
        "'{}'",
        format!("{}.{}", quote_ident(owner), quote_ident(name)).replace('\'', "''")
    );
    match action {
        "run" => format!("begin dbms_scheduler.run_job({job}, use_current_session => false); end;"),
        "stop" => format!("begin dbms_scheduler.stop_job({job}); end;"),
        "enable" => format!("begin dbms_scheduler.enable({job}); end;"),
        "disable" => format!("begin dbms_scheduler.disable({job}, force => true); end;"),
        "drop" => format!("begin dbms_scheduler.drop_job({job}, force => true); end;"),
        _ => unreachable!("unknown job action {action}"),
    }
}

fn duration(seconds: Option<f64>) -> String {
    match seconds {
        None => String::new(),
        // Run history keeps whole seconds only.
        Some(0.0) => "< 1 s".into(),
        Some(s) if s < 1.0 => format!("{:.0} ms", s * 1000.0),
        Some(s) if s < 60.0 => format!("{s:.1} s"),
        Some(s) if s < 3600.0 => format!("{}m {:02}s", (s / 60.0) as u64, (s % 60.0) as u64),
        Some(s) => format!(
            "{}h {:02}m",
            (s / 3600.0) as u64,
            ((s % 3600.0) / 60.0) as u64
        ),
    }
}

fn state_color(ui: &Ui, state: &str) -> Color32 {
    let v = ui.visuals();
    match state {
        "RUNNING" => v.hyperlink_color,
        "SCHEDULED" | "SUCCEEDED" => Color32::from_rgb(0x2f, 0xa0, 0x4c),
        "BROKEN" | "FAILED" | "STOPPED" => v.error_fg_color,
        "RETRY SCHEDULED" | "CHAIN_STALLED" => v.warn_fg_color,
        _ => v.weak_text_color(),
    }
}

impl JobsWindow {
    pub fn title(&self) -> String {
        "Scheduler jobs".into()
    }

    pub fn is_busy(&self) -> bool {
        !self.state.pending.is_empty()
    }

    #[cfg(test)]
    pub fn jobs(&self) -> &[Job] {
        &self.state.jobs
    }

    #[cfg(test)]
    pub fn select(&mut self, owner: &str, name: &str) -> Effects {
        self.state.selected = Some((owner.into(), name.into()));
        self.load_runs()
    }

    #[cfg(test)]
    pub fn runs(&self) -> &[JobRun] {
        &self.state.runs
    }

    #[cfg(test)]
    pub fn run_action(&mut self, action: &'static str) -> Effects {
        self.action(action)
    }

    pub fn refresh(&mut self) -> Effects {
        let mut effects = Effects::default();
        let id = request_id();
        let system = self.system;
        self.state.pending.insert(id, Pending::Jobs);
        self.state.loaded = true;
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            Ok(Box::new(load_jobs(conn, system)?) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects.extend(self.load_runs());
        effects
    }

    fn load_runs(&mut self) -> Effects {
        let mut effects = Effects::default();
        let Some((owner, name)) = self.state.selected.clone() else {
            return effects;
        };
        let id = request_id();
        self.state.pending.insert(id, Pending::Runs);
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            Ok(Box::new(load_runs(conn, &owner, &name)?) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    fn action(&mut self, action: &'static str) -> Effects {
        let mut effects = Effects::default();
        let Some((owner, name)) = self.state.selected.clone() else {
            return effects;
        };
        let sql = action_sql(action, &owner, &name);
        let id = request_id();
        self.state.pending.insert(id, Pending::Action(action));
        self.state.notice = None;
        self.state.error = None;
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            conn.execute(&sql, &[])?;
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        let DbEvent::TaskDone { id, result } = event else {
            return Effects::default();
        };
        let Some(pending) = self.state.pending.remove(&id) else {
            return Effects::default();
        };
        let s = &mut self.state;
        match (pending, result) {
            (_, Err(e)) => s.error = Some(e.to_string()),
            (Pending::Jobs, Ok(any)) => {
                if let Ok(jobs) = any.downcast::<Vec<Job>>() {
                    s.jobs = *jobs;
                    s.error = None;
                    if let Some((owner, name)) = &s.selected
                        && !s.jobs.iter().any(|j| j.owner == *owner && j.name == *name)
                    {
                        s.selected = None;
                        s.runs.clear();
                    }
                }
            }
            (Pending::Runs, Ok(any)) => {
                if let Ok(runs) = any.downcast::<Vec<JobRun>>() {
                    s.runs = *runs;
                }
            }
            (Pending::Action(action), Ok(_)) => {
                let name = s
                    .selected
                    .as_ref()
                    .map(|(_, n)| n.clone())
                    .unwrap_or_default();
                s.notice = Some(match action {
                    "run" => format!("{name} started"),
                    "stop" => format!("{name} stopped"),
                    "enable" => format!("{name} enabled"),
                    "disable" => format!("{name} disabled"),
                    "drop" => format!("{name} dropped"),
                    _ => String::new(),
                });
                return self.refresh();
            }
        }
        Effects::default()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        if !cx.connected {
            ui.centered_and_justified(|ui| ui.weak("Connect to see scheduler jobs"));
            return effects;
        }
        if !self.state.loaded {
            effects.extend(self.refresh());
        }
        let mut refresh = false;
        ui.horizontal(|ui| {
            if pythia_ui::icons::icon_text_button(ui, pythia_ui::icons::Icon::Refresh, "Refresh")
                .clicked()
            {
                refresh = true;
            }
            if ui
                .button("New job…")
                .on_hover_text("Opens the job form, in the schema the object browser shows")
                .clicked()
            {
                effects.commands.push(crate::commands::Command::NewJob);
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter jobs…")
                    .desired_width(180.0),
            );
            if ui
                .checkbox(&mut self.system, "Oracle-maintained")
                .on_hover_text("Also list the jobs of Oracle's own schemas")
                .changed()
            {
                refresh = true;
            }
            if self.is_busy() {
                ui.spinner();
            }
            if let Some(notice) = &self.state.notice {
                ui.weak(notice);
            }
        });
        if refresh {
            effects.extend(self.refresh());
        }
        if let Some(e) = &self.state.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.separator();

        let needle = self.filter.trim().to_lowercase();
        let jobs: Vec<usize> = (0..self.state.jobs.len())
            .filter(|&i| {
                let j = &self.state.jobs[i];
                needle.is_empty()
                    || j.name.to_lowercase().contains(&needle)
                    || j.owner.to_lowercase().contains(&needle)
                    || j.action.to_lowercase().contains(&needle)
            })
            .collect();
        if self.state.loaded && self.state.jobs.is_empty() && !self.is_busy() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No scheduler jobs").strong());
                ui.weak("Jobs you own or can see through privileges appear here.");
            });
            return effects;
        }

        let details_height = if self.state.selected.is_some() {
            (ui.available_height() * 0.5).max(200.0)
        } else {
            0.0
        };
        let list_height = (ui.available_height() - details_height).max(120.0);
        let mut clicked = None;
        ui.allocate_ui(egui::vec2(ui.available_width(), list_height), |ui| {
            self.jobs_table(ui, &jobs, &mut clicked);
        });
        if let Some(i) = clicked {
            let job = &self.state.jobs[i];
            let key = (job.owner.clone(), job.name.clone());
            if self.state.selected.as_ref() != Some(&key) {
                self.state.selected = Some(key);
                self.state.runs.clear();
                self.state.confirm_drop = false;
                effects.extend(self.load_runs());
            }
        }

        let selected = self.state.selected.as_ref().and_then(|(o, n)| {
            self.state
                .jobs
                .iter()
                .find(|j| j.owner == *o && j.name == *n)
                .cloned()
        });
        if let Some(job) = selected {
            ui.separator();
            if let Some(action) = self.details(ui, &job, cx, &mut effects) {
                // A read-only connection changes no job (#464).
                if cx.read_only {
                    self.state.error =
                        Some("Connection is read-only: jobs are left as they are".into());
                } else {
                    effects.extend(self.action(action));
                }
            }
        }
        effects
    }

    fn jobs_table(&self, ui: &mut Ui, jobs: &[usize], clicked: &mut Option<usize>) {
        let row_height = pythia_ui::row_height(ui);
        TableBuilder::new(ui)
            .id_salt("jobs")
            .striped(true)
            .resizable(true)
            .auto_shrink(false)
            .sense(egui::Sense::click())
            .column(TableColumn::initial(90.0).at_least(60.0).clip(true))
            .column(TableColumn::initial(190.0).at_least(80.0).clip(true))
            .column(TableColumn::initial(90.0))
            .column(TableColumn::initial(140.0))
            .column(TableColumn::initial(70.0))
            .column(TableColumn::initial(140.0))
            .column(TableColumn::initial(90.0))
            .column(TableColumn::remainder().at_least(80.0).clip(true))
            .header(row_height, |mut h| {
                for title in [
                    "Owner", "Job", "State", "Last run", "Took", "Next run", "Runs", "Schedule",
                ] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, jobs.len(), |mut row| {
                    let i = jobs[row.index()];
                    let job = &self.state.jobs[i];
                    let selected = self
                        .state
                        .selected
                        .as_ref()
                        .is_some_and(|(o, n)| *o == job.owner && *n == job.name);
                    row.set_selected(selected);
                    row.col(|ui| {
                        ui.label(&job.owner);
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(&job.name).strong());
                    });
                    row.col(|ui| {
                        let state = if job.enabled || job.state == "DISABLED" {
                            job.state.clone()
                        } else {
                            "DISABLED".into()
                        };
                        ui.label(RichText::new(&state).color(state_color(ui, &state)));
                    });
                    row.col(|ui| {
                        ui.label(&job.last_start);
                    });
                    row.col(|ui| {
                        ui.label(duration(job.last_seconds));
                    });
                    row.col(|ui| {
                        ui.label(&job.next_run);
                    });
                    row.col(|ui| {
                        let text = if job.failures > 0 {
                            RichText::new(format!("{} ({} failed)", job.runs, job.failures))
                                .color(ui.visuals().error_fg_color)
                        } else {
                            RichText::new(job.runs.to_string())
                        };
                        ui.label(text);
                    });
                    row.col(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&job.schedule).monospace()).truncate(),
                        )
                        .on_hover_text(&job.schedule);
                    });
                    if row.response().clicked() {
                        *clicked = Some(i);
                    }
                });
            });
    }

    /// The selected job's details and run history; returns a chosen action.
    fn details(
        &mut self,
        ui: &mut Ui,
        job: &Job,
        cx: &DocContext,
        effects: &mut Effects,
    ) -> Option<&'static str> {
        let mut action = None;
        ui.horizontal(|ui| {
            ui.heading(format!("{}.{}", job.owner, job.name));
            ui.label(RichText::new(&job.job_type).small().weak());
        });
        ui.horizontal(|ui| {
            let idle = self.state.pending.is_empty();
            ui.add_enabled_ui(idle, |ui| {
                if self.state.confirm_drop {
                    ui.label("Drop this job?");
                    if ui
                        .button(RichText::new("Drop job").color(ui.visuals().error_fg_color))
                        .clicked()
                    {
                        action = Some("drop");
                        self.state.confirm_drop = false;
                    }
                    if ui.button("Keep").clicked() {
                        self.state.confirm_drop = false;
                    }
                } else {
                    if job.state == "RUNNING" {
                        if pythia_ui::icons::icon_text_button(
                            ui,
                            pythia_ui::icons::Icon::Stop,
                            "Stop",
                        )
                        .clicked()
                        {
                            action = Some("stop");
                        }
                    } else if pythia_ui::icons::icon_text_button(
                        ui,
                        pythia_ui::icons::Icon::Execute,
                        "Run now",
                    )
                    .on_hover_text("Runs the job once in the background")
                    .clicked()
                    {
                        action = Some("run");
                    }
                    if job.enabled {
                        if ui.button("Disable").clicked() {
                            action = Some("disable");
                        }
                    } else if ui.button("Enable").clicked() {
                        action = Some("enable");
                    }
                    if let Some(connection) = cx.connection.clone()
                        && ui
                            .button("Edit…")
                            .on_hover_text("What it runs, when, and its options")
                            .clicked()
                    {
                        effects.open.push(OpenRequest::EditObject {
                            connection,
                            target: crate::object_editor::EditTarget::Edit(
                                crate::documents::ObjectRef {
                                    owner: job.owner.clone(),
                                    name: job.name.clone(),
                                    object_type: pythia_db::ObjectType::Job,
                                },
                            ),
                        });
                    }
                    if ui.button("Drop…").clicked() {
                        self.state.confirm_drop = true;
                    }
                }
            });
        });
        if !job.comments.is_empty() {
            ui.weak(&job.comments);
        }
        egui::Grid::new("job_details")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.weak("Schedule");
                ui.label(RichText::new(&job.schedule).monospace());
                ui.end_row();
                ui.weak("Action");
                ui.horizontal(|ui| {
                    let first = job.action.lines().next().unwrap_or_default();
                    let more = if job.action.lines().nth(1).is_some() {
                        " …"
                    } else {
                        ""
                    };
                    ui.add(
                        egui::Label::new(RichText::new(format!("{first}{more}")).monospace())
                            .truncate(),
                    )
                    .on_hover_text(&job.action);
                    if job.job_type == "PLSQL_BLOCK"
                        && let Some(connection) = cx.connection.clone()
                        && ui
                            .small_button("Open")
                            .on_hover_text("Open the action in a SQL window")
                            .clicked()
                    {
                        effects.open.push(OpenRequest::Sql {
                            connection,
                            text: format!(
                                "-- Action of {}.{}\n{}\n",
                                job.owner, job.name, job.action
                            ),
                            run: false,
                        });
                    }
                });
                ui.end_row();
            });
        ui.add_space(6.0);
        ui.label(RichText::new("Recent runs").strong());
        if self.state.runs.is_empty() {
            ui.weak("No runs logged");
            return action;
        }
        let row_height = pythia_ui::row_height(ui);
        TableBuilder::new(ui)
            .id_salt("job_runs")
            .striped(true)
            .resizable(true)
            .auto_shrink(false)
            .column(TableColumn::initial(160.0))
            .column(TableColumn::initial(100.0))
            .column(TableColumn::initial(80.0))
            .column(TableColumn::initial(80.0))
            .column(TableColumn::remainder().clip(true))
            .header(row_height, |mut h| {
                for title in ["Started", "Status", "Took", "Error", "Details"] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, self.state.runs.len(), |mut row| {
                    let run = &self.state.runs[row.index()];
                    row.col(|ui| {
                        ui.label(&run.start);
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(&run.status).color(state_color(ui, &run.status)));
                    });
                    row.col(|ui| {
                        ui.label(duration(run.seconds));
                    });
                    row.col(|ui| {
                        if let Some(e) = run.error {
                            ui.label(
                                RichText::new(format!("ORA-{e:05}"))
                                    .color(ui.visuals().error_fg_color),
                            );
                        }
                    });
                    row.col(|ui| {
                        let first = run.info.lines().next().unwrap_or_default();
                        ui.add(egui::Label::new(first).truncate())
                            .on_hover_text(&run.info);
                    });
                });
            });
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_action_sql() {
        assert_eq!(
            action_sql("run", "HR", "NIGHTLY"),
            "begin dbms_scheduler.run_job('HR.NIGHTLY', use_current_session => false); end;"
        );
        assert_eq!(
            action_sql("disable", "HR", "my job"),
            "begin dbms_scheduler.disable('HR.\"my job\"', force => true); end;"
        );
        assert_eq!(duration(Some(0.25)), "250 ms");
        assert_eq!(duration(Some(0.0)), "< 1 s");
        assert_eq!(duration(Some(75.0)), "1m 15s");
        assert_eq!(duration(Some(3725.0)), "1h 02m");
    }
}
