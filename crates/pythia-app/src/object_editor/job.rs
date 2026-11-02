//! Scheduler job: what it runs (a PL/SQL block or a stored procedure),
//! when (start, a repeat interval written from presets or typed, end), and
//! whether it's enabled and dropped after its last run. A new job is
//! `DBMS_SCHEDULER.CREATE_JOB`; a change sets the changed attributes.

use egui::{RichText, Ui};
use pythia_db::oracle::Connection;
use pythia_db::oracle::sql_type::{OracleType, ToSql};
use pythia_db::{DbError, Dialect};

use super::{Lists, form_grid, form_row, ident, name_field, names, picker, qualified};
use crate::browser::written;
use crate::documents::quote_ident;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Block,
    Procedure,
}

impl JobKind {
    fn job_type(self) -> &'static str {
        match self {
            JobKind::Block => "PLSQL_BLOCK",
            JobKind::Procedure => "STORED_PROCEDURE",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobDef {
    pub name: String,
    pub kind: JobKind,
    /// The block, or the procedure's name.
    pub action: String,
    /// `YYYY-MM-DD HH24:MI`; empty: as soon as it's enabled.
    pub start: String,
    /// Calendar expression (or PL/SQL date expression); empty: runs once.
    pub repeat: String,
    /// `YYYY-MM-DD HH24:MI`; empty: never.
    pub end: String,
    pub enabled: bool,
    pub auto_drop: bool,
    pub comments: String,
    /// What runs comes from elsewhere (a program, another job type): shown,
    /// not edited.
    pub fixed_action: Option<String>,
    /// When it runs comes from elsewhere (a named schedule, a window, an
    /// event): shown, not edited.
    pub fixed_schedule: Option<String>,
}

impl Default for JobDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: JobKind::Block,
            action: "begin\n  null;\nend;".into(),
            start: String::new(),
            repeat: "FREQ=DAILY;BYHOUR=2;BYMINUTE=0;BYSECOND=0".into(),
            end: String::new(),
            enabled: true,
            auto_drop: false,
            comments: String::new(),
            fixed_action: None,
            fixed_schedule: None,
        }
    }
}

const DATE_FORMAT: &str = "YYYY-MM-DD HH24:MI";

pub fn load(conn: &Connection, owner: &str, name: &str) -> Result<JobDef, DbError> {
    let row = conn
        .query(
            "select job_type, job_action, program_owner, program_name, schedule_type,
                    schedule_owner, schedule_name, to_char(start_date, 'YYYY-MM-DD HH24:MI'),
                    repeat_interval, to_char(end_date, 'YYYY-MM-DD HH24:MI'), enabled,
                    auto_drop, comments
               from all_scheduler_jobs where owner = :1 and job_name = :2",
            &[&owner, &name],
        )?
        .next()
        .transpose()?
        .ok_or_else(|| DbError::new(format!("Scheduler job {owner}.{name} not found")))?;
    // Thirteen columns: more than a tuple takes.
    let mut columns = Vec::with_capacity(13);
    for i in 0..13 {
        columns.push(row.get::<_, Option<String>>(i)?);
    }
    let mut column = columns.into_iter();
    let mut next = || column.next().flatten();
    let (job_type, action, program_owner, program, schedule_type) =
        (next(), next(), next(), next(), next());
    let (schedule_owner, schedule, start, repeat, end) = (next(), next(), next(), next(), next());
    let (enabled, auto_drop, comments) = (next(), next(), next());
    let text = |s: Option<String>| s.unwrap_or_default();
    let dotted = |owner: Option<String>, name: Option<String>| match (owner, name) {
        (Some(o), Some(n)) => format!("{o}.{n}"),
        (_, n) => text(n),
    };
    let (kind, fixed_action) = match job_type.as_deref() {
        Some("PLSQL_BLOCK") => (JobKind::Block, None),
        Some("STORED_PROCEDURE") => (JobKind::Procedure, None),
        Some(other) => (
            JobKind::Block,
            Some(format!(
                "{}: {}",
                other.to_lowercase(),
                text(action.clone())
            )),
        ),
        None => (
            JobKind::Block,
            Some(format!("program {}", dotted(program_owner, program))),
        ),
    };
    let fixed_schedule = match schedule_type.as_deref() {
        Some("NAMED") => Some(format!("schedule {}", dotted(schedule_owner, schedule))),
        Some("WINDOW") => Some(format!("window {}", text(schedule))),
        Some("WINDOW_GROUP") => Some(format!("window group {}", text(schedule))),
        Some("EVENT") => Some("an event".to_owned()),
        _ => None,
    };
    Ok(JobDef {
        name: name.to_owned(),
        kind,
        action: if fixed_action.is_some() {
            String::new()
        } else {
            text(action)
        },
        start: text(start),
        repeat: text(repeat),
        end: text(end),
        enabled: enabled.as_deref() == Some("TRUE"),
        auto_drop: auto_drop.as_deref() == Some("TRUE"),
        comments: text(comments),
        fixed_action,
        fixed_schedule,
    })
}

/// The schema's procedures, packaged ones too, and what the session may
/// do.
pub fn lists(conn: &Connection, owner: &str, lists: &mut super::BaseLists) -> Result<(), DbError> {
    // As SQL reads them, picked into the Procedure field: "MyProc" isn't
    // myproc (#365).
    lists.procedures.clear();
    for row in conn.query_as::<(String, Option<String>)>(
        "select distinct object_name, procedure_name
           from all_procedures
          where owner = :1
            and (object_type = 'PROCEDURE' and procedure_name is null
                 or object_type = 'PACKAGE' and procedure_name is not null)
          order by 1, 2",
        &[&owner],
    )? {
        let (object, procedure) = row?;
        let object = written(Dialect::ORACLE, &object);
        lists.procedures.push(match procedure {
            Some(p) => format!("{object}.{}", written(Dialect::ORACLE, &p)),
            None => object,
        });
    }
    lists.session_privileges = names(conn, "select privilege from session_privs", &[])?;
    lists.session_user = conn.query_row_as::<String>("select user from dual", &[])?;
    Ok(())
}

/// The next runs of a calendar expression, from `start` (empty: now).
pub fn next_runs(conn: &Connection, start: &str, repeat: &str) -> Result<Vec<String>, DbError> {
    let mut stmt = conn
        .statement(
            "declare
               l_start timestamp with time zone := systimestamp;
               l_after timestamp with time zone := systimestamp;
               l_next  timestamp with time zone;
               l_out   varchar2(4000);
             begin
               if :start_at is not null then
                 l_start := to_timestamp_tz(:start_at, :format);
                 if l_start > l_after then
                   l_after := l_start - interval '1' second;
                 end if;
               end if;
               for i in 1 .. 5 loop
                 dbms_scheduler.evaluate_calendar_string(:calendar, l_start, l_after, l_next);
                 exit when l_next is null;
                 l_out := l_out || to_char(l_next, 'Dy YYYY-MM-DD HH24:MI',
                                           'nls_date_language = english') || chr(10);
                 l_after := l_next;
               end loop;
               :runs := l_out;
             end;",
        )
        .build()?;
    let start = (!start.trim().is_empty()).then(|| start.trim().to_owned());
    let binds: [(&str, &dyn ToSql); 4] = [
        ("start_at", &start),
        ("format", &DATE_FORMAT),
        ("calendar", &repeat.trim()),
        ("runs", &OracleType::Varchar2(4000)),
    ];
    stmt.execute_named(&binds)?;
    let runs: Option<String> = stmt.bind_value("runs")?;
    Ok(runs
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect())
}

/// A date as typed, or why not.
fn date_sql(label: &str, value: &str) -> Result<String, String> {
    let v = value.trim();
    let digits = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_digit());
    let (date, time) = v.split_once(' ').unwrap_or((v, ""));
    let mut parts = date.split('-');
    let date_ok = matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(y), Some(m), Some(d), None) if digits(y, 4) && digits(m, 2) && digits(d, 2)
    );
    let time_ok = time.is_empty()
        || matches!(time.trim().split_once(':'),
            Some((h, m)) if (digits(h, 1) || digits(h, 2)) && digits(m, 2));
    if !date_ok || !time_ok {
        return Err(format!("{label}: a date like 2026-10-23 02:00"));
    }
    let format = if time.is_empty() {
        "YYYY-MM-DD"
    } else {
        DATE_FORMAT
    };
    Ok(format!("to_timestamp_tz('{v}', '{format}')"))
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A procedure's name as written: plain parts upper case.
fn procedure_name(name: &str) -> Result<String, String> {
    let mut parts = Vec::new();
    for part in name.trim().split('.') {
        let quoted = part.len() > 2 && part.starts_with('"') && part.ends_with('"');
        let plain = part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_$#".contains(c))
            && part.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
        if !quoted && !plain {
            return Err("The procedure: a name like cleanup or pkg.cleanup".into());
        }
        parts.push(quote_ident(&ident(part)));
    }
    if parts.len() > 3 {
        return Err("The procedure: a name like cleanup or pkg.cleanup".into());
    }
    Ok(parts.join("."))
}

/// The action as passed to DBMS_SCHEDULER.
fn action_value(def: &JobDef) -> Result<String, String> {
    match def.kind {
        JobKind::Block => {
            let block = def.action.trim();
            if block.is_empty() {
                return Err("Enter the PL/SQL block".into());
            }
            if !block.ends_with(';') {
                return Err("End the PL/SQL block with ;".into());
            }
            Ok(block.to_owned())
        }
        JobKind::Procedure => {
            if def.action.trim().is_empty() {
                return Err("Pick the procedure".into());
            }
            procedure_name(&def.action)
        }
    }
}

pub fn statements(
    owner: &str,
    original: Option<&JobDef>,
    def: &JobDef,
) -> Result<Vec<String>, String> {
    if def.name.trim().is_empty() {
        return Err("Enter a name".into());
    }
    let action = match &def.fixed_action {
        Some(_) => None,
        None => Some(action_value(def)?),
    };
    let start = (!def.start.trim().is_empty())
        .then(|| date_sql("Start", &def.start))
        .transpose()?;
    let end = (!def.end.trim().is_empty())
        .then(|| date_sql("End", &def.end))
        .transpose()?;
    let repeat = def.repeat.trim();
    let job = literal(&qualified(owner, &def.name));
    let Some(o) = original else {
        let mut args = vec![
            ("job_name", job),
            ("job_type", literal(def.kind.job_type())),
            ("job_action", literal(action.as_deref().unwrap_or_default())),
        ];
        if let Some(start) = start {
            args.push(("start_date", start));
        }
        if !repeat.is_empty() {
            args.push(("repeat_interval", literal(repeat)));
        }
        if let Some(end) = end {
            args.push(("end_date", end));
        }
        args.push(("enabled", def.enabled.to_string()));
        args.push(("auto_drop", def.auto_drop.to_string()));
        if !def.comments.trim().is_empty() {
            args.push(("comments", literal(def.comments.trim())));
        }
        let args: Vec<String> = args
            .into_iter()
            .map(|(name, value)| format!("    {name:<15} => {value}"))
            .collect();
        return Ok(vec![format!(
            "begin\n  dbms_scheduler.create_job(\n{});\nend;",
            args.join(",\n")
        )]);
    };
    let mut calls = Vec::new();
    let set = |attribute: &str, value: String| {
        format!("dbms_scheduler.set_attribute({job}, '{attribute}', {value});")
    };
    let clear =
        |attribute: &str| format!("dbms_scheduler.set_attribute_null({job}, '{attribute}');");
    // Disabled first, so nothing below runs it; enabled last.
    if o.enabled && !def.enabled {
        calls.push(format!("dbms_scheduler.disable({job}, force => true);"));
    }
    if let Some(action) = &action {
        if def.kind != o.kind {
            calls.push(set("job_type", literal(def.kind.job_type())));
        }
        if def.kind != o.kind || def.action.trim() != o.action.trim() {
            calls.push(set("job_action", literal(action)));
        }
    }
    let changed = |a: &str, b: &str| a.trim() != b.trim();
    if changed(&def.start, &o.start) {
        calls.push(match &start {
            Some(start) => set("start_date", start.clone()),
            None => clear("start_date"),
        });
    }
    if def.fixed_schedule.is_none() && changed(&def.repeat, &o.repeat) {
        calls.push(if repeat.is_empty() {
            clear("repeat_interval")
        } else {
            set("repeat_interval", literal(repeat))
        });
    }
    if changed(&def.end, &o.end) {
        calls.push(match &end {
            Some(end) => set("end_date", end.clone()),
            None => clear("end_date"),
        });
    }
    if def.auto_drop != o.auto_drop {
        calls.push(set("auto_drop", def.auto_drop.to_string()));
    }
    if changed(&def.comments, &o.comments) {
        calls.push(if def.comments.trim().is_empty() {
            clear("comments")
        } else {
            set("comments", literal(def.comments.trim()))
        });
    }
    if !o.enabled && def.enabled {
        calls.push(format!("dbms_scheduler.enable({job});"));
    }
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![format!("begin\n  {}\nend;", calls.join("\n  "))])
}

/// What the session can't do that the form needs.
pub fn missing_privilege(owner: &str, existing: bool, lists: &Lists) -> Option<String> {
    if lists.session_privileges.is_empty() || lists.session_user.is_empty() {
        return None;
    }
    let has = |p: &str| lists.session_privileges.iter().any(|s| s == p);
    if owner == lists.session_user {
        return (!existing && !has("CREATE JOB") && !has("CREATE ANY JOB"))
            .then(|| "This session doesn't have CREATE JOB.".to_owned());
    }
    (!has("CREATE ANY JOB")).then(|| {
        if existing {
            "Changing another schema's job takes CREATE ANY JOB (or ALTER on the job).".to_owned()
        } else {
            "Creating a job in another schema takes CREATE ANY JOB.".to_owned()
        }
    })
}

/// A repeat interval the form writes, read back from its calendar string.
#[derive(Debug, Clone, PartialEq)]
enum Preset {
    Once,
    Minutes(u32),
    Hours(u32),
    Daily(u32, u32),
    Weekly(Vec<&'static str>, u32, u32),
    Monthly(u32, u32, u32),
    Custom,
}

const DAYS: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

impl Preset {
    const CHOICES: [(&'static str, Preset); 6] = [
        ("once", Preset::Once),
        ("every few minutes", Preset::Minutes(15)),
        ("every few hours", Preset::Hours(1)),
        ("daily", Preset::Daily(2, 0)),
        ("weekly", Preset::Weekly(Vec::new(), 2, 0)),
        ("monthly", Preset::Monthly(1, 2, 0)),
    ];

    fn label(&self) -> &'static str {
        match self {
            Preset::Once => "once",
            Preset::Minutes(_) => "every few minutes",
            Preset::Hours(_) => "every few hours",
            Preset::Daily(..) => "daily",
            Preset::Weekly(..) => "weekly",
            Preset::Monthly(..) => "monthly",
            Preset::Custom => "custom",
        }
    }

    fn parse(repeat: &str) -> Preset {
        let repeat = repeat.trim();
        if repeat.is_empty() {
            return Preset::Once;
        }
        let mut fields = std::collections::BTreeMap::new();
        for part in repeat.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let Some((k, v)) = part.split_once('=') else {
                return Preset::Custom;
            };
            fields.insert(k.trim().to_uppercase(), v.trim().to_uppercase());
        }
        let number = |k: &str| fields.get(k).and_then(|v| v.parse::<u32>().ok());
        let keys: Vec<&str> = fields.keys().map(String::as_str).collect();
        let time = || Some((number("BYHOUR")?, number("BYMINUTE")?));
        let at = |rest: &[&str]| {
            let mut want: Vec<&str> = ["BYHOUR", "BYMINUTE", "BYSECOND", "FREQ"]
                .into_iter()
                .chain(rest.iter().copied())
                .collect();
            want.sort_unstable();
            keys == want && number("BYSECOND") == Some(0)
        };
        let interval = || match fields.get("INTERVAL") {
            None => Some(1),
            Some(_) => number("INTERVAL").filter(|n| *n > 0),
        };
        let only_interval = keys.iter().all(|k| matches!(*k, "FREQ" | "INTERVAL"));
        let preset = match fields.get("FREQ").map(String::as_str) {
            Some("MINUTELY") if only_interval => interval().map(Preset::Minutes),
            Some("HOURLY") if only_interval => interval().map(Preset::Hours),
            Some("DAILY") if at(&[]) => time().map(|(h, m)| Preset::Daily(h, m)),
            Some("WEEKLY") if at(&["BYDAY"]) => {
                let days: Option<Vec<&'static str>> = fields["BYDAY"]
                    .split(',')
                    .map(|d| DAYS.iter().copied().find(|x| *x == d.trim()))
                    .collect();
                days.zip(time()).map(|(mut days, (h, m))| {
                    days.sort_by_key(|d| DAYS.iter().position(|x| x == d));
                    days.dedup();
                    Preset::Weekly(days, h, m)
                })
            }
            Some("MONTHLY") if at(&["BYMONTHDAY"]) => number("BYMONTHDAY")
                .zip(time())
                .map(|(d, (h, m))| Preset::Monthly(d, h, m)),
            _ => None,
        };
        preset.unwrap_or(Preset::Custom)
    }

    /// The calendar string; `None` for a custom one (kept as typed).
    fn calendar(&self) -> Option<String> {
        let at = |h: &u32, m: &u32| format!("BYHOUR={h};BYMINUTE={m};BYSECOND=0");
        Some(match self {
            Preset::Once => String::new(),
            Preset::Minutes(n) => format!("FREQ=MINUTELY;INTERVAL={n}"),
            Preset::Hours(n) => format!("FREQ=HOURLY;INTERVAL={n}"),
            Preset::Daily(h, m) => format!("FREQ=DAILY;{}", at(h, m)),
            Preset::Weekly(days, h, m) => {
                let days = if days.is_empty() {
                    "MON".to_owned()
                } else {
                    days.join(",")
                };
                format!("FREQ=WEEKLY;BYDAY={days};{}", at(h, m))
            }
            Preset::Monthly(d, h, m) => format!("FREQ=MONTHLY;BYMONTHDAY={d};{}", at(h, m)),
            Preset::Custom => return None,
        })
    }
}

/// Hours and minutes of a time of day.
fn time_ui(ui: &mut Ui, h: &mut u32, m: &mut u32) {
    ui.label("at");
    ui.add(
        egui::DragValue::new(h)
            .range(0..=23)
            .custom_formatter(|v, _| format!("{v:02}")),
    );
    ui.label(":");
    ui.add(
        egui::DragValue::new(m)
            .range(0..=59)
            .custom_formatter(|v, _| format!("{v:02}")),
    );
}

/// The repeat interval: a preset and its settings, and the calendar
/// string it writes (or that's typed).
fn repeat_ui(ui: &mut Ui, repeat: &mut String) {
    let mut preset = Preset::parse(repeat);
    let before = preset.clone();
    egui::ComboBox::from_id_salt("job-repeat")
        .selected_text(preset.label())
        .show_ui(ui, |ui| {
            for (label, choice) in Preset::CHOICES {
                if ui
                    .selectable_label(preset.label() == label, label)
                    .clicked()
                    && preset.label() != label
                {
                    preset = choice;
                }
            }
        });
    let unit = if matches!(preset, Preset::Minutes(_)) {
        "minutes"
    } else {
        "hours"
    };
    match &mut preset {
        Preset::Minutes(n) | Preset::Hours(n) => {
            ui.label("every");
            ui.add(egui::DragValue::new(n).range(1..=999));
            ui.label(unit);
        }
        Preset::Daily(h, m) => time_ui(ui, h, m),
        Preset::Weekly(days, h, m) => {
            ui.label("on");
            for day in DAYS {
                let mut on = days.contains(&day);
                let label = format!("{}{}", &day[..1], day[1..].to_lowercase());
                if ui.toggle_value(&mut on, label).changed() {
                    if on {
                        days.push(day);
                        days.sort_by_key(|d| DAYS.iter().position(|x| x == d));
                    } else {
                        days.retain(|d| *d != day);
                    }
                }
            }
            time_ui(ui, h, m);
        }
        Preset::Monthly(d, h, m) => {
            ui.label("on day");
            ui.add(egui::DragValue::new(d).range(1..=31));
            time_ui(ui, h, m);
        }
        Preset::Once | Preset::Custom => {}
    }
    if preset != before
        && let Some(calendar) = preset.calendar()
    {
        *repeat = calendar;
    }
}

/// When the job runs next, as far as the database says.
fn next_runs_ui(ui: &mut Ui, def: &JobDef, lists: &mut Lists) {
    if def.fixed_schedule.is_some() {
        return;
    }
    let repeat = def.repeat.trim();
    if repeat.is_empty() {
        ui.weak(if def.start.trim().is_empty() {
            "Runs once, as soon as it's enabled.".to_owned()
        } else {
            format!("Runs once, at {}.", def.start.trim())
        });
        return;
    }
    if !repeat.to_uppercase().starts_with("FREQ=") {
        ui.weak("A PL/SQL date expression: it's evaluated after each run.");
        return;
    }
    let key = (def.start.trim().to_owned(), repeat.to_owned());
    // Asked for once typing pauses.
    let id = ui.id().with("job-next-runs");
    let now = ui.input(|i| i.time);
    let stored: Option<((String, String), f64)> = ui.data(|d| d.get_temp(id));
    let since = match stored {
        Some((k, t)) if k == key => t,
        Some(_) => {
            ui.data_mut(|d| d.insert_temp(id, (key.clone(), now)));
            now
        }
        // Shown for the first time: no need to wait.
        None => {
            ui.data_mut(|d| d.insert_temp(id, (key.clone(), f64::NEG_INFINITY)));
            f64::NEG_INFINITY
        }
    };
    match lists.next_runs.get(&key) {
        Some(Ok(runs)) if runs.is_empty() => {
            ui.weak("It doesn't run again (the calendar has no more dates).");
        }
        Some(Ok(runs)) => {
            ui.horizontal_wrapped(|ui| {
                ui.weak("Next runs:");
                for run in runs {
                    ui.label(RichText::new(run).monospace().small());
                }
            });
        }
        Some(Err(e)) => {
            ui.colored_label(ui.visuals().warn_fg_color, e);
        }
        None if now - since >= 0.4 => {
            lists.want_next_runs = Some(key);
            ui.weak("Next runs…");
        }
        None => {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(450));
            ui.weak("Next runs…");
        }
    }
}

pub fn ui(ui: &mut Ui, def: &mut JobDef, existing: bool, owner: &str, lists: &mut Lists) {
    if let Some(missing) = missing_privilege(owner, existing, lists) {
        ui.colored_label(ui.visuals().warn_fg_color, missing);
        ui.add_space(4.0);
    }
    let height = ui.spacing().interact_size.y;
    form_grid(ui, "job", |ui| {
        name_field(ui, &mut def.name, existing);
        match &def.fixed_action {
            Some(fixed) => form_row(ui, "Runs", |ui| {
                ui.label(RichText::new(fixed).monospace());
            }),
            None => {
                form_row(ui, "Runs", |ui| {
                    ui.radio_value(&mut def.kind, JobKind::Block, "PL/SQL block");
                    ui.radio_value(&mut def.kind, JobKind::Procedure, "stored procedure")
                        .on_hover_text("A procedure without parameters (or with defaults)");
                });
                match def.kind {
                    JobKind::Block => form_row(ui, "Block", |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut def.action)
                                .code_editor()
                                .desired_rows(4)
                                .desired_width(460.0),
                        );
                    }),
                    JobKind::Procedure => form_row(ui, "Procedure", |ui| {
                        ui.add_sized(
                            [260.0, height],
                            egui::TextEdit::singleline(&mut def.action)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("pkg.procedure"),
                        );
                        if let Some(p) = picker(ui, "job-procedure", &lists.procedures) {
                            def.action = p;
                        }
                    }),
                }
            }
        }
        form_row(ui, "Start", |ui| {
            ui.add_sized(
                [160.0, height],
                egui::TextEdit::singleline(&mut def.start).hint_text("when enabled"),
            )
            .on_hover_text("YYYY-MM-DD HH:MI in the session's time zone");
        });
        match &def.fixed_schedule {
            Some(fixed) => form_row(ui, "Schedule", |ui| {
                ui.label(fixed);
            }),
            None => {
                form_row(ui, "Repeat", |ui| repeat_ui(ui, &mut def.repeat));
                form_row(ui, "Calendar", |ui| {
                    ui.add_sized(
                        [460.0, height],
                        egui::TextEdit::singleline(&mut def.repeat)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("once"),
                    )
                    .on_hover_text(
                        "A calendar expression (FREQ=…), or a PL/SQL date expression; empty runs once",
                    );
                });
            }
        }
        form_row(ui, "End", |ui| {
            ui.add_sized(
                [160.0, height],
                egui::TextEdit::singleline(&mut def.end).hint_text("never"),
            )
            .on_hover_text("YYYY-MM-DD HH:MI: no runs after it");
        });
        form_row(ui, "Options", |ui| {
            ui.checkbox(&mut def.enabled, "enabled");
            ui.checkbox(&mut def.auto_drop, "drop after its last run");
        });
        form_row(ui, "Comments", |ui| {
            ui.add(egui::TextEdit::singleline(&mut def.comments).desired_width(460.0));
        });
    });
    ui.add_space(6.0);
    next_runs_ui(ui, def, lists);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_jobs() {
        let def = JobDef {
            name: "nightly_cleanup".into(),
            action: "begin cleanup('it''s'); end;".into(),
            start: "2026-10-23 02:00".into(),
            comments: "Old rows".into(),
            ..Default::default()
        };
        assert_eq!(
            statements("HR", None, &def).unwrap(),
            ["begin\n  dbms_scheduler.create_job(\n    \
              job_name        => 'HR.NIGHTLY_CLEANUP',\n    \
              job_type        => 'PLSQL_BLOCK',\n    \
              job_action      => 'begin cleanup(''it''''s''); end;',\n    \
              start_date      => to_timestamp_tz('2026-10-23 02:00', 'YYYY-MM-DD HH24:MI'),\n    \
              repeat_interval => 'FREQ=DAILY;BYHOUR=2;BYMINUTE=0;BYSECOND=0',\n    \
              enabled         => true,\n    \
              auto_drop       => false,\n    \
              comments        => 'Old rows');\nend;"]
        );
        let once = JobDef {
            name: "once".into(),
            kind: JobKind::Procedure,
            action: "pkg.Run_It".into(),
            repeat: String::new(),
            enabled: false,
            auto_drop: true,
            ..Default::default()
        };
        assert_eq!(
            statements("HR", None, &once).unwrap(),
            ["begin\n  dbms_scheduler.create_job(\n    \
              job_name        => 'HR.ONCE',\n    \
              job_type        => 'STORED_PROCEDURE',\n    \
              job_action      => 'PKG.RUN_IT',\n    \
              enabled         => false,\n    \
              auto_drop       => true);\nend;"]
        );
    }

    #[test]
    fn sets_changed_attributes() {
        let original = JobDef {
            name: "NIGHTLY".into(),
            action: "begin cleanup; end;".into(),
            start: "2026-10-23 02:00".into(),
            comments: "Old rows".into(),
            ..Default::default()
        };
        assert!(
            statements("HR", Some(&original), &original)
                .unwrap()
                .is_empty()
        );
        let mut edited = original.clone();
        edited.enabled = false;
        edited.start = String::new();
        edited.repeat = "FREQ=HOURLY;INTERVAL=4".into();
        edited.end = "2027-01-01".into();
        edited.kind = JobKind::Procedure;
        edited.action = "cleanup".into();
        edited.comments = String::new();
        edited.auto_drop = true;
        assert_eq!(
            statements("HR", Some(&original), &edited).unwrap(),
            ["begin\n  \
              dbms_scheduler.disable('HR.NIGHTLY', force => true);\n  \
              dbms_scheduler.set_attribute('HR.NIGHTLY', 'job_type', 'STORED_PROCEDURE');\n  \
              dbms_scheduler.set_attribute('HR.NIGHTLY', 'job_action', 'CLEANUP');\n  \
              dbms_scheduler.set_attribute_null('HR.NIGHTLY', 'start_date');\n  \
              dbms_scheduler.set_attribute('HR.NIGHTLY', 'repeat_interval', 'FREQ=HOURLY;INTERVAL=4');\n  \
              dbms_scheduler.set_attribute('HR.NIGHTLY', 'end_date', to_timestamp_tz('2027-01-01', 'YYYY-MM-DD'));\n  \
              dbms_scheduler.set_attribute('HR.NIGHTLY', 'auto_drop', true);\n  \
              dbms_scheduler.set_attribute_null('HR.NIGHTLY', 'comments');\nend;"]
        );
        // A job with a named schedule keeps it; enabling comes last.
        let named = JobDef {
            enabled: false,
            fixed_schedule: Some("schedule HR.NIGHTS".into()),
            repeat: String::new(),
            ..original.clone()
        };
        let mut edited = named.clone();
        edited.enabled = true;
        edited.repeat = "FREQ=DAILY".into();
        edited.action = "begin cleanup(1); end;".into();
        assert_eq!(
            statements("HR", Some(&named), &edited).unwrap(),
            ["begin\n  \
              dbms_scheduler.set_attribute('HR.NIGHTLY', 'job_action', 'begin cleanup(1); end;');\n  \
              dbms_scheduler.enable('HR.NIGHTLY');\nend;"]
        );
    }

    #[test]
    fn checks_the_form() {
        let check = |f: &dyn Fn(&mut JobDef)| {
            let mut def = JobDef {
                name: "j".into(),
                ..Default::default()
            };
            f(&mut def);
            statements("HR", None, &def).unwrap_err()
        };
        assert_eq!(check(&|d| d.name.clear()), "Enter a name");
        assert_eq!(check(&|d| d.action = "  ".into()), "Enter the PL/SQL block");
        assert_eq!(
            check(&|d| d.action = "begin null; end".into()),
            "End the PL/SQL block with ;"
        );
        assert_eq!(
            check(&|d| {
                d.kind = JobKind::Procedure;
                d.action = "do it".into();
            }),
            "The procedure: a name like cleanup or pkg.cleanup"
        );
        assert_eq!(
            check(&|d| d.start = "tomorrow".into()),
            "Start: a date like 2026-10-23 02:00"
        );
        assert_eq!(
            check(&|d| d.end = "2026-1-1".into()),
            "End: a date like 2026-10-23 02:00"
        );
    }

    #[test]
    fn reads_and_writes_presets() {
        let cases = [
            ("", Preset::Once),
            ("FREQ=MINUTELY;INTERVAL=15", Preset::Minutes(15)),
            ("freq=hourly", Preset::Hours(1)),
            (
                "FREQ=DAILY;BYHOUR=2;BYMINUTE=30;BYSECOND=0",
                Preset::Daily(2, 30),
            ),
            (
                "FREQ=WEEKLY; BYDAY=FRI,MON; BYHOUR=7; BYMINUTE=0; BYSECOND=0",
                Preset::Weekly(vec!["MON", "FRI"], 7, 0),
            ),
            (
                "FREQ=MONTHLY;BYMONTHDAY=1;BYHOUR=0;BYMINUTE=5;BYSECOND=0",
                Preset::Monthly(1, 0, 5),
            ),
            ("FREQ=DAILY;BYHOUR=2", Preset::Custom),
            ("FREQ=MINUTELY;INTERVAL=0", Preset::Custom),
            (
                "FREQ=WEEKLY;BYDAY=XYZ;BYHOUR=7;BYMINUTE=0;BYSECOND=0",
                Preset::Custom,
            ),
            ("sysdate + 1", Preset::Custom),
        ];
        for (calendar, preset) in cases {
            assert_eq!(Preset::parse(calendar), preset, "{calendar}");
        }
        assert_eq!(
            Preset::Weekly(vec!["MON", "FRI"], 7, 0).calendar().unwrap(),
            "FREQ=WEEKLY;BYDAY=MON,FRI;BYHOUR=7;BYMINUTE=0;BYSECOND=0"
        );
        for (_, preset) in Preset::CHOICES {
            let calendar = preset.calendar().unwrap();
            let parsed = Preset::parse(&calendar);
            assert_eq!(parsed.label(), preset.label(), "{calendar}");
        }
        assert_eq!(Preset::Custom.calendar(), None);
    }
}
