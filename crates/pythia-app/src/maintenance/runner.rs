//! Runs maintenance statements one at a time on the window's own session,
//! with what each answered and how long it took. A batch (#145) groups
//! them by object: a failing statement skips the rest of its object's, and
//! the batch goes on with the next object; Break stops it all. While a
//! statement runs, the progress the database reports is read on the
//! connection's metadata session: Oracle's `V$SESSION_LONGOPS`,
//! PostgreSQL's `pg_stat_progress_*`, SQL Server's `percent_complete`.

use std::time::{Duration, Instant};

use egui::{Color32, RichText, Ui};
use pythia_db::{Client, DbEvent, DbKind, DbRequest, Dialect};

use crate::documents::{Effects, ObjectRef, Target, request_id};

/// Rows of a statement's answer kept (MySQL's ANALYZE TABLE and friends
/// answer with a few).
const MAX_ROWS: usize = 200;

/// How often the progress is read while a statement runs, and after how
/// long it starts.
const POLL: Duration = Duration::from_secs(1);

/// A statement's object (group) and how it went, for tests.
#[cfg(test)]
pub type Ended = (usize, &'static str);

/// A statement to run: its object's group, and the table (or index) whose
/// progress the database reports.
pub struct Planned {
    pub group: usize,
    pub sql: String,
    pub target: Option<ObjectRef>,
}

#[derive(Default)]
pub struct Runner {
    steps: Vec<Step>,
    /// What the groups are (a batch's objects); none for a single object.
    groups: Vec<String>,
    run: Option<Run>,
    /// The step that ended last, which trailing messages belong to.
    ended: Option<usize>,
    /// How the last run ended, and whether it went well.
    outcome: Option<(String, bool)>,
}

struct Run {
    /// What runs, for the outcome.
    label: String,
    /// The step running, and its request.
    step: usize,
    id: u64,
    columns: Vec<String>,
    started: Instant,
    began: Instant,
    cancelled: bool,
    /// What the database reports of the running statement.
    progress: Option<String>,
    /// The progress request waiting for its answer, and when it was sent.
    polling: Option<u64>,
    polled: Option<Instant>,
}

struct Step {
    group: usize,
    sql: String,
    target: Option<ObjectRef>,
    status: Status,
    /// Notices, output and result rows.
    messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Waiting,
    Running,
    Done(Duration),
    Failed(Duration, String),
    Skipped,
}

impl Runner {
    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    pub fn cancelling(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.cancelled)
    }

    /// Cancels the running statement; the rest don't run.
    pub fn cancel(&mut self) {
        if let Some(run) = self.run.as_mut() {
            run.cancelled = true;
        }
    }

    #[cfg(test)]
    pub fn outcome(&self) -> Option<&(String, bool)> {
        self.outcome.as_ref()
    }

    /// Starts running `steps`, the first one now. `groups` names the objects
    /// of a batch (empty for one object).
    pub fn start(&mut self, label: String, groups: Vec<String>, steps: Vec<Planned>) -> Effects {
        let mut effects = Effects::default();
        if steps.is_empty() || self.run.is_some() {
            return effects;
        }
        self.outcome = None;
        self.ended = None;
        self.groups = groups;
        self.steps = steps
            .into_iter()
            .map(|p| Step {
                group: p.group,
                sql: p.sql,
                target: p.target,
                status: Status::Waiting,
                messages: Vec::new(),
            })
            .collect();
        let now = Instant::now();
        self.run = Some(Run {
            label,
            step: 0,
            id: 0,
            columns: Vec::new(),
            started: now,
            began: now,
            cancelled: false,
            progress: None,
            polling: None,
            polled: None,
        });
        self.send_step(0, &mut effects);
        effects
    }

    fn send_step(&mut self, step: usize, effects: &mut Effects) {
        let (Some(run), Some(s)) = (self.run.as_mut(), self.steps.get_mut(step)) else {
            return;
        };
        let id = request_id();
        run.step = step;
        run.id = id;
        run.columns.clear();
        run.started = Instant::now();
        run.progress = None;
        run.polled = None;
        s.status = Status::Running;
        effects.request(
            Target::Own,
            DbRequest::Execute {
                id,
                sql: s.sql.clone(),
                max_rows: MAX_ROWS,
                binds: Vec::new(),
            },
        );
    }

    /// The running step ended: the next one starts (after a failure, the
    /// next object's first), or the run is over.
    fn step_ended(&mut self, failed: Option<String>, effects: &mut Effects) {
        let Some(run) = self.run.as_ref() else {
            return;
        };
        let (step, elapsed, cancelled) = (run.step, run.started.elapsed(), run.cancelled);
        self.ended = Some(step);
        let group = self.steps.get(step).map_or(0, |s| s.group);
        if let Some(s) = self.steps.get_mut(step) {
            match &failed {
                Some(e) => s.status = Status::Failed(elapsed, e.clone()),
                None => {
                    s.status = Status::Done(elapsed);
                    effects.executed.push(s.sql.clone());
                }
            }
        }
        let batch = !self.groups.is_empty();
        let next = if cancelled || (failed.is_some() && !batch) {
            None
        } else {
            // A failure skips the rest of its object's statements.
            let mut next = step + 1;
            while failed.is_some() && self.steps.get(next).is_some_and(|s| s.group == group) {
                self.steps[next].status = Status::Skipped;
                next += 1;
            }
            (next < self.steps.len()).then_some(next)
        };
        if let Some(next) = next {
            self.send_step(next, effects);
            return;
        }
        let Some(run) = self.run.take() else {
            return;
        };
        for s in &mut self.steps[step + 1..] {
            if s.status == Status::Waiting {
                s.status = Status::Skipped;
            }
        }
        let (label, secs) = (run.label, run.began.elapsed().as_secs_f64());
        self.outcome = Some(if batch {
            self.batch_outcome(&label, secs, cancelled)
        } else {
            match (failed, cancelled) {
                (_, true) => (format!("{label}: cancelled after {secs:.1} s"), false),
                (Some(e), _) => (format!("{label}: failed after {secs:.1} s: {e}"), false),
                (None, _) => (format!("{label}: done in {secs:.1} s"), true),
            }
        });
    }

    /// "3 done, 1 failed in 4.2 s": a batch's objects by how they went.
    fn batch_outcome(&self, label: &str, secs: f64, cancelled: bool) -> (String, bool) {
        let (mut done, mut failed, mut not_run) = (0, 0, 0);
        for group in 0..self.groups.len() {
            let mut steps = self.steps.iter().filter(|s| s.group == group).peekable();
            if steps.peek().is_none() {
                continue;
            }
            let statuses: Vec<&Status> = steps.map(|s| &s.status).collect();
            if statuses.iter().any(|s| matches!(s, Status::Failed(..))) {
                failed += 1;
            } else if statuses.iter().all(|s| matches!(s, Status::Done(_))) {
                done += 1;
            } else {
                not_run += 1;
            }
        }
        let mut parts = vec![format!("{done} done")];
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if not_run > 0 {
            parts.push(format!("{not_run} not run"));
        }
        let how = if cancelled { "cancelled after" } else { "in" };
        (
            format!("{label}: {} {how} {secs:.1} s", parts.join(", ")),
            failed == 0 && not_run == 0 && !cancelled,
        )
    }

    /// Handles the run's events; true when the run ended with this one.
    pub fn on_event(&mut self, event: DbEvent, dialect: Dialect, effects: &mut Effects) -> bool {
        let running = self.run.as_ref().map(|r| r.id);
        let running_step = self.run.as_ref().map(|r| r.step);
        let polling = self.run.as_ref().and_then(|r| r.polling);
        let was_running = running.is_some();
        match event {
            DbEvent::TaskDone { id, result } if polling == Some(id) => {
                if let Some(run) = self.run.as_mut() {
                    run.polling = None;
                    run.progress = result
                        .ok()
                        .and_then(|any| any.downcast::<Option<String>>().ok())
                        .and_then(|p| *p);
                }
                return false;
            }
            DbEvent::QueryStarted { id, columns } if running == Some(id) => {
                if let Some(run) = self.run.as_mut() {
                    run.columns = columns.into_iter().map(|c| c.name).collect();
                }
                return false;
            }
            DbEvent::Rows { id, rows, .. } if running == Some(id) => {
                // A query's answer ends with its rows (no more are fetched).
                let columns = self
                    .run
                    .as_ref()
                    .map(|r| r.columns.clone())
                    .unwrap_or_default();
                let (lines, error) = answer(&columns, &rows);
                if let Some(step) = running_step {
                    self.add_messages(step, lines);
                }
                self.step_ended(error, effects);
            }
            DbEvent::Executed { id, .. } if running == Some(id) => {
                self.step_ended(None, effects);
            }
            DbEvent::Failed { id, error, .. } if running == Some(id) => {
                self.step_ended(Some(error.to_string()), effects);
            }
            DbEvent::Error(error) | DbEvent::ConnectFailed(error) if was_running => {
                self.step_ended(Some(error.to_string()), effects);
            }
            DbEvent::Notice(text) => {
                self.message(dialect, running_step, vec![text]);
                return false;
            }
            DbEvent::Output(lines) => {
                self.message(dialect, running_step, lines);
                return false;
            }
            _ => return false,
        }
        was_running && self.run.is_none()
    }

    /// Notices and output go with their statement. PostgreSQL's notices and
    /// MySQL's warnings come after the event that ends it (when the next
    /// statement may run already); Oracle's output and SQL Server's
    /// messages before.
    fn message(&mut self, dialect: Dialect, running: Option<usize>, lines: Vec<String>) {
        let trailing = matches!(dialect.kind(), DbKind::Postgres | DbKind::MySql);
        if let Some(step) = if trailing { self.ended } else { running } {
            self.add_messages(step, lines);
        }
    }

    fn add_messages(&mut self, step: usize, lines: Vec<String>) {
        if let Some(s) = self.steps.get_mut(step) {
            s.messages.extend(lines);
        }
    }

    /// Asks for the running statement's progress, once a second after its
    /// first.
    pub fn poll(&mut self, dialect: Dialect) -> Effects {
        let mut effects = Effects::default();
        let Some(run) = self.run.as_mut() else {
            return effects;
        };
        let due = run.started.elapsed() >= POLL && run.polled.is_none_or(|p| p.elapsed() >= POLL);
        if run.polling.is_some() || !due {
            return effects;
        }
        let Some(step) = self.steps.get(run.step) else {
            return effects;
        };
        let Some(task) = progress(dialect, step.target.clone(), step.sql.clone()) else {
            return effects;
        };
        let id = request_id();
        run.polling = Some(id);
        run.polled = Some(Instant::now());
        effects.request(Target::Meta, DbRequest::Run { id, task });
        effects
    }

    pub fn ui(&self, ui: &mut Ui) {
        if self.steps.is_empty() {
            return;
        }
        ui.add_space(6.0);
        let green = Color32::from_rgb(0x2f, 0xa0, 0x4c);
        if let Some((text, ok)) = &self.outcome {
            let color = if *ok {
                green
            } else {
                ui.visuals().error_fg_color
            };
            ui.label(RichText::new(text).color(color));
        }
        egui::ScrollArea::vertical()
            .id_salt("maintenance-run")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let mut group = None;
                for step in &self.steps {
                    if group != Some(step.group)
                        && let Some(label) = self.groups.get(step.group)
                    {
                        group = Some(step.group);
                        ui.label(RichText::new(label).strong());
                    }
                    ui.horizontal(|ui| {
                        let (mark, color, time) = match &step.status {
                            Status::Waiting => ("·", ui.visuals().weak_text_color(), None),
                            Status::Running => ("…", ui.visuals().text_color(), None),
                            Status::Done(d) => ("✔", green, Some(*d)),
                            Status::Failed(d, _) => ("✖", ui.visuals().error_fg_color, Some(*d)),
                            Status::Skipped => ("–", ui.visuals().weak_text_color(), None),
                        };
                        ui.label(RichText::new(mark).color(color));
                        let first = step.sql.lines().find(|l| {
                            let l = l.trim();
                            !l.is_empty() && l != "begin"
                        });
                        ui.label(RichText::new(first.unwrap_or_default().trim()).monospace());
                        match (&step.status, time) {
                            (Status::Running, _) => {
                                if let Some(run) = &self.run {
                                    ui.weak(format!(
                                        "{:.1} s",
                                        run.started.elapsed().as_secs_f64()
                                    ));
                                    if let Some(p) = &run.progress {
                                        ui.weak(p);
                                    }
                                }
                                ui.spinner();
                            }
                            (_, Some(d)) => {
                                ui.weak(format!("{:.1} s", d.as_secs_f64()));
                            }
                            (Status::Skipped, _) => {
                                ui.weak("not run");
                            }
                            _ => {}
                        }
                    });
                    if let Status::Failed(_, e) = &step.status {
                        ui.indent(("failed", &step.sql), |ui| {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        });
                    }
                    if !step.messages.is_empty() {
                        ui.indent(("messages", &step.sql), |ui| {
                            for m in &step.messages {
                                ui.label(RichText::new(m).small());
                            }
                        });
                    }
                }
            });
        if self.run.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Each statement's messages, for tests.
    #[cfg(test)]
    pub fn messages(&self) -> Vec<Vec<String>> {
        self.steps.iter().map(|s| s.messages.clone()).collect()
    }

    /// Each statement's group and how it went (`done`, `failed`, `not run`),
    /// for tests.
    #[cfg(test)]
    pub fn statuses(&self) -> Vec<Ended> {
        self.steps
            .iter()
            .map(|s| {
                let status = match s.status {
                    Status::Done(_) => "done",
                    Status::Failed(..) => "failed",
                    Status::Skipped => "not run",
                    Status::Waiting | Status::Running => "pending",
                };
                (s.group, status)
            })
            .collect()
    }
}

/// What came back as rows: MySQL's `Msg_type: Msg_text` lines (an error
/// among them fails the statement), other answers cell by cell.
pub fn answer(columns: &[String], rows: &[Vec<pythia_db::Cell>]) -> (Vec<String>, Option<String>) {
    let at = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let mut lines = Vec::new();
    let mut error = None;
    match (at("Msg_type"), at("Msg_text")) {
        (Some(kind), Some(text)) => {
            for row in rows {
                let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
                let (kind, text) = (cell(kind), cell(text));
                if kind.eq_ignore_ascii_case("error") && error.is_none() {
                    error = Some(text.clone());
                }
                lines.push(format!("{kind}: {text}"));
            }
        }
        _ => {
            for row in rows {
                lines.push(
                    row.iter()
                        .map(|c| c.as_deref().unwrap_or("NULL"))
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
        }
    }
    (lines, error)
}

/// Reads what the database reports of `sql` running on `target`: none
/// where it reports nothing (MySQL) or the target is unknown.
fn progress(dialect: Dialect, target: Option<ObjectRef>, sql: String) -> Option<pythia_db::RunFn> {
    let kind = dialect.kind();
    if matches!(kind, DbKind::MySql | DbKind::Sqlite)
        || (target.is_none() && kind != DbKind::SqlServer)
    {
        return None;
    }
    Some(Box::new(move |client: &mut dyn Client| {
        let report = match kind {
            DbKind::Oracle => target.and_then(|t| oracle_progress(client, &t)),
            DbKind::Postgres => target.and_then(|t| postgres_progress(client, &t)),
            _ => sqlserver_progress(client, &sql),
        };
        Ok(Box::new(report) as Box<dyn std::any::Any + Send>)
    }))
}

fn cell(row: &[pythia_db::Cell], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn number(row: &[pythia_db::Cell], i: usize) -> f64 {
    cell(row, i).parse().unwrap_or(0.0)
}

/// `42 % (scanning heap)`.
fn percent(done: f64, total: f64, what: &str) -> Option<String> {
    (total > 0.0).then(|| {
        let what = what.to_lowercase();
        format!("{:.0} % ({what})", 100.0 * done / total)
    })
}

/// The newest of the user's long operations on the object or its table.
fn oracle_progress(client: &mut dyn Client, target: &ObjectRef) -> Option<String> {
    let name = format!("{}.{}", target.owner, target.name);
    let rows = client
        .query(
            "select opname, sofar, totalwork from v$session_longops \
              where sofar < totalwork and username = user \
                and (target = :1 or target like :2) \
              order by last_update_time desc",
            &[&name, &format!("{name}.%")],
        )
        .ok()?;
    let row = rows.first()?;
    percent(number(row, 1), number(row, 2), &cell(row, 0))
}

/// What `pg_stat_progress_*` says of the relation: VACUUM, VACUUM FULL,
/// ANALYZE (13+), CREATE INDEX and REINDEX (12+). Views a release doesn't
/// have are skipped.
fn postgres_progress(client: &mut dyn Client, target: &ObjectRef) -> Option<String> {
    let name = Dialect::POSTGRES.qualified(&target.owner, &target.name);
    [
        "select phase, heap_blks_scanned, heap_blks_total from pg_stat_progress_vacuum \
          where relid = to_regclass($1)",
        "select phase, heap_blks_scanned, heap_blks_total from pg_stat_progress_cluster \
          where relid = to_regclass($1)",
        "select phase, sample_blks_scanned, sample_blks_total from pg_stat_progress_analyze \
          where relid = to_regclass($1)",
        "select phase, blocks_done, blocks_total from pg_stat_progress_create_index \
          where relid = to_regclass($1) or index_relid = to_regclass($1)",
    ]
    .iter()
    .find_map(|sql| {
        let rows = client.query(sql, &[&name]).ok()?;
        let row = rows.first()?;
        let phase = cell(row, 0);
        percent(number(row, 1), number(row, 2), &phase).or(Some(phase))
    })
}

/// The request running exactly `sql`, from `sys.dm_exec_requests` (which
/// shows other sessions' requests with VIEW SERVER STATE).
fn sqlserver_progress(client: &mut dyn Client, sql: &str) -> Option<String> {
    let rows = client
        .query(
            "select r.percent_complete, r.command from sys.dm_exec_requests r \
              cross apply sys.dm_exec_sql_text(r.sql_handle) t \
              where t.text = @P1 and r.session_id <> @@spid",
            &[sql],
        )
        .ok()?;
    let row = rows.first()?;
    let pct = number(row, 0);
    (pct > 0.0).then(|| format!("{pct:.0} % ({})", cell(row, 1).to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(group: usize, sql: &str) -> Planned {
        Planned {
            group,
            sql: sql.into(),
            target: None,
        }
    }

    /// The id of the request the runner sent last.
    fn sent(effects: &Effects) -> u64 {
        match effects.requests.last() {
            Some((_, DbRequest::Execute { id, .. })) => *id,
            other => panic!("no statement sent: {other:?}"),
        }
    }

    fn done(id: u64) -> DbEvent {
        DbEvent::Executed {
            id,
            summary: pythia_db::ExecSummary {
                kind: "ALTER".into(),
                rows_affected: None,
                compiled: None,
            },
            elapsed: Duration::ZERO,
        }
    }

    fn failed(id: u64) -> DbEvent {
        DbEvent::Failed {
            id,
            error: pythia_db::DbError::new("ORA-01418: specified index does not exist"),
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn a_batch_goes_on_with_the_next_object_after_a_failure() {
        let mut r = Runner::default();
        let mut effects = r.start(
            "Batch".into(),
            vec!["a".into(), "b".into(), "c".into()],
            vec![
                planned(0, "one"),
                planned(1, "two"),
                planned(1, "two more"),
                planned(2, "three"),
            ],
        );
        let oracle = Dialect::ORACLE;
        let id = sent(&effects);
        assert!(!r.on_event(done(id), oracle, &mut effects));
        let id = sent(&effects);
        // The second object's first statement fails: its second is skipped.
        assert!(!r.on_event(failed(id), oracle, &mut effects));
        let id = sent(&effects);
        assert!(r.on_event(done(id), oracle, &mut effects));
        assert_eq!(
            r.statuses(),
            [(0, "done"), (1, "failed"), (1, "not run"), (2, "done")]
        );
        let (text, ok) = r.outcome().unwrap();
        assert!(text.starts_with("Batch: 2 done, 1 failed in "), "{text}");
        assert!(!ok);
    }

    #[test]
    fn one_object_stops_at_a_failure() {
        let mut r = Runner::default();
        let mut effects = r.start(
            "Rebuild".into(),
            Vec::new(),
            vec![planned(0, "one"), planned(0, "two")],
        );
        let id = sent(&effects);
        assert!(r.on_event(failed(id), Dialect::ORACLE, &mut effects));
        assert_eq!(r.statuses(), [(0, "failed"), (0, "not run")]);
        assert!(r.outcome().unwrap().0.starts_with("Rebuild: failed after "));
    }

    #[test]
    fn cancelling_a_batch_stops_it() {
        let mut r = Runner::default();
        let mut effects = r.start(
            "Batch".into(),
            vec!["a".into(), "b".into()],
            vec![planned(0, "one"), planned(1, "two")],
        );
        r.cancel();
        let id = sent(&effects);
        assert!(r.on_event(failed(id), Dialect::POSTGRES, &mut effects));
        assert_eq!(r.statuses(), [(0, "failed"), (1, "not run")]);
        let (text, ok) = r.outcome().unwrap();
        assert!(text.contains("cancelled after"), "{text}");
        assert!(!ok);
    }

    #[test]
    fn mysql_answers_are_messages_and_errors() {
        let columns: Vec<String> = ["Table", "Op", "Msg_type", "Msg_text"]
            .map(String::from)
            .to_vec();
        let row = |kind: &str, text: &str| {
            vec![
                Some("shop.orders".to_owned()),
                Some("optimize".to_owned()),
                Some(kind.to_owned()),
                Some(text.to_owned()),
            ]
        };
        let (lines, error) = answer(
            &columns,
            &[
                row(
                    "note",
                    "Table does not support optimize, doing recreate + analyze instead",
                ),
                row("status", "OK"),
            ],
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "status: OK");
        assert_eq!(error, None);
        let (_, error) = answer(
            &columns,
            &[
                row("Error", "Table 'shop.nope' doesn't exist"),
                row("status", "Operation failed"),
            ],
        );
        assert_eq!(error.as_deref(), Some("Table 'shop.nope' doesn't exist"));
    }

    #[test]
    fn progress_in_percent() {
        assert_eq!(
            percent(42.0, 100.0, "SCANNING HEAP").as_deref(),
            Some("42 % (scanning heap)")
        );
        assert_eq!(percent(1.0, 0.0, "x"), None);
    }
}
