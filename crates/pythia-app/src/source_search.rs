//! Find in database source: searches ALL_SOURCE (text or regular
//! expression) and opens hits in program windows at the matching line.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{RichText, Ui};
use pythia_db::oracle::Connection;
use pythia_db::{DbError, DbEvent, DbRequest, Dialect, ObjectType};
use serde::{Deserialize, Serialize};

use crate::documents::{DocContext, Effects, ObjectRef, OpenRequest, Target, request_id};

/// One ALL_SOURCE line: owner, name, type, line, text.
type SourceRow = (String, String, String, u32, String);

/// More hits than this are cut off (the query fetches one extra to know).
const MAX_HITS: usize = 2000;

#[derive(Default, Serialize, Deserialize)]
pub struct SourceSearch {
    pub query: String,
    pub regex: bool,
    pub match_case: bool,
    /// Restrict to one schema (empty: all visible schemas).
    pub owner: String,
    pub include_oracle: bool,
    /// Object name filter, `*` and `?` as wildcards (empty: all).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(skip)]
    state: State,
}

/// What to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SourceKind {
    /// Stored PL/SQL, views and job actions.
    #[default]
    All,
    Packages,
    Subprograms,
    Types,
    Triggers,
    Views,
    Jobs,
}

impl SourceKind {
    const ALL: [SourceKind; 7] = [
        SourceKind::All,
        SourceKind::Packages,
        SourceKind::Subprograms,
        SourceKind::Types,
        SourceKind::Triggers,
        SourceKind::Views,
        SourceKind::Jobs,
    ];

    fn label(self) -> &'static str {
        match self {
            SourceKind::All => "Everything",
            SourceKind::Packages => "Packages",
            SourceKind::Subprograms => "Procedures and functions",
            SourceKind::Types => "Types",
            SourceKind::Triggers => "Triggers",
            SourceKind::Views => "Views",
            SourceKind::Jobs => "Scheduler job actions",
        }
    }

    /// ALL_SOURCE types searched, or `None` for none.
    fn source_types(self) -> Option<&'static str> {
        match self {
            SourceKind::All => Some(""),
            SourceKind::Packages => Some(" and s.type in ('PACKAGE', 'PACKAGE BODY')"),
            SourceKind::Subprograms => Some(" and s.type in ('PROCEDURE', 'FUNCTION')"),
            SourceKind::Types => Some(" and s.type in ('TYPE', 'TYPE BODY')"),
            SourceKind::Triggers => Some(" and s.type = 'TRIGGER'"),
            SourceKind::Views | SourceKind::Jobs => None,
        }
    }
}

#[derive(Default)]
struct State {
    pending: Option<(u64, Instant)>,
    result: Option<Result<SearchResult, String>>,
    focus: bool,
}

struct SearchResult {
    /// Hits grouped by object, in dictionary order.
    objects: BTreeMap<(String, String, String), Vec<Hit>>,
    total: usize,
    truncated: bool,
    elapsed: Duration,
    /// The pattern that produced these hits, for highlighting.
    pattern: String,
    /// Full actions of the jobs with hits, to open them.
    job_actions: HashMap<(String, String), String>,
}

#[derive(Debug, Clone)]
struct Hit {
    line: u32,
    text: String,
}

impl SourceSearch {
    pub fn new() -> Self {
        Self {
            state: State {
                focus: true,
                ..State::default()
            },
            ..Self::default()
        }
    }

    pub fn title(&self) -> String {
        if self.query.is_empty() {
            "Find in source".into()
        } else {
            format!("Find: {}", self.query)
        }
    }

    pub fn is_busy(&self) -> bool {
        self.state.pending.is_some()
    }

    pub fn search(&mut self) -> Effects {
        let mut effects = Effects::default();
        let pattern = self.query.trim().to_owned();
        if pattern.is_empty() || self.is_busy() {
            return effects;
        }
        let (regex, match_case, include_oracle, kind) =
            (self.regex, self.match_case, self.include_oracle, self.kind);
        // As Oracle reads it: `"my_schema"` as written (#337).
        let owner = Some(Dialect::ORACLE.typed_name(&self.owner)).filter(|o| !o.is_empty());
        let name = Some(self.name.trim())
            .filter(|n| !n.is_empty())
            .map(|n| n.replace('*', "%").replace('?', "_"));
        let id = request_id();
        self.state.pending = Some((id, Instant::now()));
        let task = Box::new(move |conn: &Connection| {
            let filter = Filter {
                regex,
                match_case,
                owner: owner.as_deref(),
                name: name.as_deref(),
                include_oracle,
                kind,
            };
            let hits = query(conn, &pattern, &filter)?;
            Ok(Box::new((pattern, hits)) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        let DbEvent::TaskDone { id, result } = event else {
            return Effects::default();
        };
        let Some((_, started)) = self.state.pending.filter(|(p, _)| *p == id) else {
            return Effects::default();
        };
        self.state.pending = None;
        type Answer = (String, Found);
        self.state.result = Some(match result.map(|any| any.downcast::<Answer>()) {
            Ok(Ok(found)) => {
                let (pattern, Found { rows, job_actions }) = *found;
                let truncated = rows.len() > MAX_HITS;
                let mut objects: BTreeMap<_, Vec<Hit>> = BTreeMap::new();
                for (owner, name, ty, line, text) in rows.into_iter().take(MAX_HITS) {
                    objects
                        .entry((owner, name, ty))
                        .or_default()
                        .push(Hit { line, text });
                }
                let total = objects.values().map(Vec::len).sum();
                Ok(SearchResult {
                    objects,
                    total,
                    truncated,
                    elapsed: started.elapsed(),
                    pattern,
                    job_actions,
                })
            }
            Ok(Err(_)) => Err("unexpected result".into()),
            Err(e) => Err(e.to_string()),
        });
        Effects::default()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        let mut run = false;
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text(if self.regex {
                        "Regular expression, e.g. raise_application_error\\(-20\\d{3}"
                    } else {
                        "Text to find in PL/SQL source"
                    })
                    .desired_width(420.0),
            );
            if std::mem::take(&mut self.state.focus) {
                edit.request_focus();
            }
            run |= edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let can = cx.connected && !self.is_busy() && !self.query.trim().is_empty();
            if ui
                .add_enabled(
                    can,
                    egui::Button::image_and_text(pythia_ui::icons::Icon::Search.image(ui), "Find"),
                )
                .clicked()
            {
                run = true;
            }
            if self.is_busy() {
                ui.spinner();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.match_case, "Match case");
            ui.checkbox(&mut self.regex, "Regular expression");
            ui.checkbox(&mut self.include_oracle, "Include Oracle schemas");
            ui.label("Schema");
            ui.add(
                egui::TextEdit::singleline(&mut self.owner)
                    .hint_text("all")
                    .desired_width(120.0),
            );
            ui.label("Name");
            ui.add(
                egui::TextEdit::singleline(&mut self.name)
                    .hint_text("all, or e.g. ORDER_*")
                    .desired_width(140.0),
            );
            egui::ComboBox::from_id_salt("source-kind")
                .selected_text(self.kind.label())
                .show_ui(ui, |ui| {
                    for k in SourceKind::ALL {
                        ui.selectable_value(&mut self.kind, k, k.label());
                    }
                });
        });
        if run && cx.connected {
            effects.extend(self.search());
        }
        ui.separator();

        match &self.state.result {
            None => {
                ui.weak("Searches the source of packages, procedures, functions, types and triggers, view queries and scheduler job actions you can see.");
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            Some(Ok(result)) => {
                let summary = format!(
                    "{} match{} in {} object{} · {} ms{}",
                    result.total,
                    if result.total == 1 { "" } else { "es" },
                    result.objects.len(),
                    if result.objects.len() == 1 { "" } else { "s" },
                    result.elapsed.as_millis(),
                    if result.truncated {
                        format!(" · first {MAX_HITS} shown")
                    } else {
                        String::new()
                    },
                );
                ui.weak(summary);
                ui.add_space(4.0);
                let highlight = (!self.regex).then(|| (result.pattern.clone(), self.match_case));
                let mut open = None;
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for ((owner, name, ty), hits) in &result.objects {
                            let header = format!(
                                "{}.{}  ·  {}  ·  {}",
                                owner,
                                name,
                                ty.to_lowercase(),
                                hits.len()
                            );
                            egui::CollapsingHeader::new(RichText::new(header).strong())
                                .id_salt((owner, name, ty))
                                .default_open(result.objects.len() <= 30)
                                .show(ui, |ui| {
                                    for hit in hits {
                                        let job = line_job(ui, hit, highlight.as_ref());
                                        if ui
                                            .add(egui::Button::new(job).frame_when_inactive(false))
                                            .clicked()
                                        {
                                            open = Some((
                                                owner.clone(),
                                                name.clone(),
                                                ty.clone(),
                                                hit.line,
                                            ));
                                        }
                                    }
                                });
                        }
                    });
                if let (Some((owner, name, ty, line)), Some(connection)) =
                    (open, cx.connection.clone())
                {
                    match ty.as_str() {
                        "JOB" => effects.open.push(OpenRequest::Sql {
                            connection,
                            text: format!(
                                "-- Action of scheduler job {owner}.{name}\n{}\n",
                                result
                                    .job_actions
                                    .get(&(owner.clone(), name.clone()))
                                    .map(String::as_str)
                                    .unwrap_or_default()
                            ),
                            run: false,
                        }),
                        "VIEW" => effects.open.push(OpenRequest::Object {
                            connection,
                            object: ObjectRef {
                                owner,
                                name,
                                object_type: ObjectType::View,
                            },
                        }),
                        _ => {
                            if let Some(object_type) = ObjectType::from_dictionary(&ty) {
                                effects.open.push(OpenRequest::ProgramAt {
                                    connection,
                                    object: ObjectRef {
                                        owner,
                                        name,
                                        object_type,
                                    },
                                    line: line as usize,
                                });
                            }
                        }
                    }
                }
            }
        }
        effects
    }
}

/// `line  text` with the plain-text match highlighted.
fn line_job(ui: &Ui, hit: &Hit, highlight: Option<&(String, bool)>) -> LayoutJob {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let weak = TextFormat {
        font_id: font.clone(),
        color: ui.visuals().weak_text_color(),
        ..Default::default()
    };
    let normal = TextFormat {
        font_id: font.clone(),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    let mark = TextFormat {
        background: ui.visuals().selection.bg_fill,
        ..normal.clone()
    };
    let mut job = LayoutJob::default();
    job.append(&format!("{:>6}  ", hit.line), 0.0, weak);
    let text = hit.text.trim_end().replace('\t', "    ");
    // Highlight every occurrence of a plain-text pattern.
    let mut pos = 0;
    if let Some((pattern, case)) = highlight.filter(|(p, _)| !p.is_empty()) {
        let hay = if *case {
            text.clone()
        } else {
            text.to_lowercase()
        };
        let needle = if *case {
            pattern.clone()
        } else {
            pattern.to_lowercase()
        };
        // Lowercasing can change byte lengths; only highlight when it didn't.
        if hay.len() == text.len() {
            while let Some(found) = hay[pos..].find(&needle) {
                let start = pos + found;
                let end = start + needle.len();
                job.append(&text[pos..start], 0.0, normal.clone());
                job.append(&text[start..end], 0.0, mark.clone());
                pos = end;
            }
        }
    }
    job.append(&text[pos..], 0.0, normal);
    job
}

struct Filter<'a> {
    regex: bool,
    match_case: bool,
    owner: Option<&'a str>,
    /// LIKE pattern on the object name, in any case (#305).
    name: Option<&'a str>,
    include_oracle: bool,
    kind: SourceKind,
}

/// Hits, plus the full text of matching job actions.
#[derive(Default)]
struct Found {
    rows: Vec<SourceRow>,
    job_actions: HashMap<(String, String), String>,
}

fn query(conn: &Connection, pattern: &str, f: &Filter) -> Result<Found, DbError> {
    let condition = |col: &str| match (f.regex, f.match_case) {
        (true, true) => format!("regexp_like({col}, :pattern, 'c')"),
        (true, false) => format!("regexp_like({col}, :pattern, 'i')"),
        (false, true) => format!("instr({col}, :pattern) > 0"),
        (false, false) => format!("instr(upper({col}), upper(:pattern)) > 0"),
    };
    let release = pythia_db::compat::release(conn)?;
    let oracle_filter = |col: &str| {
        if f.include_oracle {
            String::new()
        } else {
            format!(
                " and {col} not in ({})",
                pythia_db::compat::schemas_query(release, true)
            )
        }
    };
    let binds: [(&str, &dyn pythia_db::oracle::sql_type::ToSql); 3] = [
        ("pattern", &pattern),
        ("owner", &f.owner),
        ("name", &f.name),
    ];
    let mut found = Found::default();
    if let Some(types) = f.kind.source_types() {
        let sql = format!(
            "select s.owner, s.name, s.type, s.line, s.text from all_source s \
              where {} and (:owner is null or s.owner = :owner) \
                and (:name is null or upper(s.name) like upper(:name)){types}{} \
              order by s.owner, s.name, s.type, s.line",
            condition("s.text"),
            oracle_filter("s.owner"),
        );
        let sql = pythia_db::compat::first_rows(&sql, MAX_HITS + 1);
        for row in conn.query_named(&sql, &binds)? {
            let row = row?;
            let (owner, name, ty, line, text): (String, String, String, u32, Option<String>) =
                row.get_as()?;
            found
                .rows
                .push((owner, name, ty, line, text.unwrap_or_default()));
        }
    }
    // Views and job actions are whole texts: find the matching lines here.
    let line_matches = line_matcher(pattern, f.regex, f.match_case);
    let mut whole = |owner: String, name: String, ty: &str, text: String| {
        let mut any = false;
        for (i, line) in text.lines().enumerate() {
            if line_matches(line) {
                found.rows.push((
                    owner.clone(),
                    name.clone(),
                    ty.to_owned(),
                    i as u32 + 1,
                    line.to_owned(),
                ));
                any = true;
            }
        }
        // Oracle matched but not on one line (e.g. a regex across lines).
        if !any {
            let first = text.lines().next().unwrap_or_default().to_owned();
            found
                .rows
                .push((owner.clone(), name.clone(), ty.to_owned(), 1, first));
        }
        if ty == "JOB" {
            found.job_actions.insert((owner, name), text);
        }
    };
    if matches!(f.kind, SourceKind::All | SourceKind::Views) && release >= (12, 1) {
        let sql = format!(
            "select v.owner, v.view_name, v.text_vc from all_views v \
              where {} and (:owner is null or v.owner = :owner) \
                and (:name is null or upper(v.view_name) like upper(:name)){} \
              order by v.owner, v.view_name",
            condition("v.text_vc"),
            oracle_filter("v.owner")
        );
        let sql = pythia_db::compat::first_rows(&sql, 500);
        for row in conn.query_named(&sql, &binds)? {
            let (owner, name, text): (String, String, Option<String>) = row?.get_as()?;
            whole(owner, name, "VIEW", text.unwrap_or_default());
        }
    } else if matches!(f.kind, SourceKind::All | SourceKind::Views) {
        // Before 12.1 the text is only a LONG, which SQL can't search:
        // match its lines here.
        let sql = format!(
            "select v.owner, v.view_name, v.text from all_views v \
              where (:owner is null or v.owner = :owner) \
                and (:name is null or upper(v.view_name) like upper(:name)){} \
              order by v.owner, v.view_name",
            oracle_filter("v.owner")
        );
        let mut hits = 0;
        for row in conn.query_named(&sql, &binds[1..])? {
            let (owner, name, text): (String, String, Option<String>) = row?.get_as()?;
            let text = text.unwrap_or_default();
            if text.lines().any(&line_matches) {
                whole(owner, name, "VIEW", text);
                hits += 1;
                if hits == 500 {
                    break;
                }
            }
        }
    }
    if matches!(f.kind, SourceKind::All | SourceKind::Jobs) {
        let sql = format!(
            "select j.owner, j.job_name, j.job_action from all_scheduler_jobs j \
              where {} and (:owner is null or j.owner = :owner) \
                and (:name is null or upper(j.job_name) like upper(:name)){} \
              order by j.owner, j.job_name",
            condition("j.job_action"),
            oracle_filter("j.owner")
        );
        let sql = pythia_db::compat::first_rows(&sql, 500);
        for row in conn.query_named(&sql, &binds)? {
            let (owner, name, text): (String, String, Option<String>) = row?.get_as()?;
            whole(owner, name, "JOB", text.unwrap_or_default());
        }
    }
    Ok(found)
}

/// Matches single lines client-side like the database did for the whole text.
fn line_matcher(pattern: &str, regex: bool, match_case: bool) -> Box<dyn Fn(&str) -> bool> {
    if regex {
        match regex::RegexBuilder::new(pattern)
            .case_insensitive(!match_case)
            .build()
        {
            Ok(re) => Box::new(move |line| re.is_match(line)),
            Err(_) => Box::new(|_| false),
        }
    } else if match_case {
        let needle = pattern.to_owned();
        Box::new(move |line| line.contains(&needle))
    } else {
        let needle = pattern.to_lowercase();
        Box::new(move |line| line.to_lowercase().contains(&needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_matching() {
        assert!(line_matcher("Foo", false, false)("a foo b"));
        assert!(!line_matcher("Foo", false, true)("a foo b"));
        assert!(line_matcher(
            r"raise_application_error\(-20\d{3}",
            true,
            false
        )("  RAISE_APPLICATION_ERROR(-20001, 'x');"));
    }

    #[test]
    fn searches_views_and_job_actions() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        // Before 23ai, a new user can't create jobs.
        crate::test_db::as_dba(&["grant create job to {user}"]);
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        conn.execute(
            "create or replace view pythia_ss_v as\nselect 'needle_xq7' as marker\n  from dual",
            &[],
        )
        .unwrap();
        conn.execute(
            "begin
               for j in (select job_name from user_scheduler_jobs where job_name = 'PYTHIA_SS_JOB') loop
                 dbms_scheduler.drop_job(j.job_name, force => true);
               end loop;
               dbms_scheduler.create_job(job_name => 'PYTHIA_SS_JOB', job_type => 'PLSQL_BLOCK',
                 job_action => 'begin' || chr(10) || '  null; -- needle_xq7' || chr(10) || 'end;',
                 enabled => false);
             end;",
            &[],
        )
        .unwrap();
        let filter = Filter {
            regex: false,
            match_case: false,
            owner: None,
            name: None,
            include_oracle: false,
            kind: SourceKind::All,
        };
        let found = query(&conn, "NEEDLE_XQ7", &filter).unwrap();
        let hit = |ty: &str| {
            found
                .rows
                .iter()
                .find(|r| r.2 == ty)
                .map(|r| (r.1.clone(), r.3))
        };
        // The stored text starts at the query, not at CREATE VIEW.
        assert_eq!(hit("VIEW"), Some(("PYTHIA_SS_V".into(), 1)));
        assert_eq!(hit("JOB"), Some(("PYTHIA_SS_JOB".into(), 2)));
        assert!(found.job_actions.values().any(|a| a.starts_with("begin")));

        let only_views = Filter {
            kind: SourceKind::Views,
            name: Some("PYTHIA_SS%"),
            ..filter
        };
        let found = query(&conn, "needle_xq7", &only_views).unwrap();
        assert!(found.rows.iter().all(|r| r.2 == "VIEW"));
        assert_eq!(found.rows.len(), 1);

        conn.execute("drop view pythia_ss_v", &[]).unwrap();
        conn.execute(
            "begin dbms_scheduler.drop_job('PYTHIA_SS_JOB', force => true); end;",
            &[],
        )
        .unwrap();
    }

    /// The name filter finds a quoted name with lowercase letters by any
    /// case (#305).
    #[test]
    fn the_name_filter_finds_names_in_any_case() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        conn.execute(
            "create or replace procedure \"R62_Proc\" is begin null; -- needle_r62\nend;",
            &[],
        )
        .unwrap();
        conn.execute(
            "create or replace view \"R62_View\" as select 'needle_r62' as marker from dual",
            &[],
        )
        .unwrap();
        let names = |name: &str| {
            let filter = Filter {
                regex: false,
                match_case: false,
                owner: None,
                name: Some(name),
                include_oracle: false,
                kind: SourceKind::All,
            };
            let found = query(&conn, "needle_r62", &filter).unwrap();
            found
                .rows
                .into_iter()
                .map(|r| format!("{} {}", r.2, r.1))
                .collect::<Vec<_>>()
        };
        assert_eq!(names("r62_p%"), ["PROCEDURE R62_Proc"]);
        assert_eq!(names("R62_VIEW"), ["VIEW R62_View"]);
        conn.execute("drop procedure \"R62_Proc\"", &[]).unwrap();
        conn.execute("drop view \"R62_View\"", &[]).unwrap();
    }
}
