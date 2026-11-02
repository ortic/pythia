//! Materialized view: query, build, refresh (method, when, schedule) and
//! query rewrite. Refresh and rewrite change in place; a new query
//! recreates the view.

use egui::Ui;
use pythia_db::DbError;
use pythia_db::oracle::Connection;

use super::{Lists, form_grid, form_row, ident, name_field, picker, qualified};
use crate::documents::quote_ident;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Refresh {
    #[default]
    Force,
    Complete,
    Fast,
    Never,
}

impl Refresh {
    fn keyword(self) -> &'static str {
        match self {
            Refresh::Force => "force",
            Refresh::Complete => "complete",
            Refresh::Fast => "fast",
            Refresh::Never => "never",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MviewDef {
    pub name: String,
    pub query: String,
    /// Empty: the default tablespace (new views only).
    pub tablespace: String,
    /// Filled by the first refresh instead of when created.
    pub build_deferred: bool,
    pub refresh: Refresh,
    /// Refreshed by each commit on the tables (instead of on demand).
    pub on_commit: bool,
    /// Automatic refreshes on demand: first time and interval as date
    /// expressions (`sysdate + 1/24`); empty: none (or unchanged).
    pub start: String,
    pub next: String,
    pub rewrite: bool,
}

impl MviewDef {
    pub fn new() -> Self {
        Self {
            query: "select *\n  from dual".into(),
            ..Self::default()
        }
    }
}

pub fn load(conn: &Connection, owner: &str, name: &str) -> Result<MviewDef, DbError> {
    let (query, method, mode, rewrite, build) = conn
        .query_row_as::<(Option<String>, String, String, String, String)>(
            "select query, refresh_method, refresh_mode, rewrite_enabled, build_mode
               from all_mviews where owner = :1 and mview_name = :2",
            &[&owner, &name],
        )
        .map_err(|e| match e.db_error().map(|d| d.code()) {
            Some(1403) => DbError::new(format!("Materialized view {owner}.{name} not found")),
            _ => e.into(),
        })?;
    Ok(MviewDef {
        name: quote_ident(name),
        query: query.unwrap_or_default().trim().to_owned(),
        tablespace: String::new(),
        build_deferred: build == "DEFERRED",
        refresh: match method.as_str() {
            "COMPLETE" => Refresh::Complete,
            "FAST" => Refresh::Fast,
            "NEVER" => Refresh::Never,
            _ => Refresh::Force,
        },
        on_commit: mode == "COMMIT",
        start: String::new(),
        next: String::new(),
        rewrite: rewrite == "Y",
    })
}

/// `refresh fast on demand start with … next …`, or `never refresh`.
fn refresh_clause(def: &MviewDef) -> String {
    if def.refresh == Refresh::Never {
        return "never refresh".into();
    }
    let when = if def.on_commit {
        "on commit"
    } else {
        "on demand"
    };
    let mut clause = format!("refresh {} {when}", def.refresh.keyword());
    if !def.on_commit {
        if !def.start.trim().is_empty() {
            clause.push_str(&format!(" start with {}", def.start.trim()));
        }
        if !def.next.trim().is_empty() {
            clause.push_str(&format!(" next {}", def.next.trim()));
        }
    }
    clause
}

fn create(owner: &str, def: &MviewDef, query: &str) -> String {
    let mut clauses = Vec::new();
    if !def.tablespace.trim().is_empty() {
        clauses.push(format!(
            "tablespace {}",
            quote_ident(&ident(&def.tablespace))
        ));
    }
    clauses.push(if def.build_deferred {
        "build deferred".to_owned()
    } else {
        "build immediate".to_owned()
    });
    clauses.push(refresh_clause(def));
    if def.rewrite {
        clauses.push("enable query rewrite".into());
    }
    format!(
        "create materialized view {}\n  {}\nas\n{query}",
        qualified(owner, &def.name),
        clauses.join("\n  ")
    )
}

pub fn statements(
    owner: &str,
    original: Option<&MviewDef>,
    def: &MviewDef,
) -> Result<Vec<String>, String> {
    if def.name.trim().is_empty() {
        return Err("Enter a name".into());
    }
    let query = def.query.trim().trim_end_matches(';').trim_end();
    if query.is_empty() {
        return Err("Enter the query".into());
    }
    if def.on_commit && (!def.start.trim().is_empty() || !def.next.trim().is_empty()) {
        return Err("A view refreshed on commit has no schedule".into());
    }
    let Some(original) = original else {
        return Ok(vec![create(owner, def, query)]);
    };
    let name = qualified(owner, &def.name);
    if query != original.query.trim() {
        // The query is checked first, without fetching a row: a mistake in
        // it would otherwise fail the create after the drop.
        return Ok(vec![
            format!("-- The new query is checked first\nselect 1 from (\n{query}\n) where 1 = 0"),
            format!("drop materialized view {name}"),
            create(owner, def, query),
        ]);
    }
    let mut out = Vec::new();
    let schedule = !def.start.trim().is_empty() || !def.next.trim().is_empty();
    if def.refresh != original.refresh || def.on_commit != original.on_commit || schedule {
        out.push(format!(
            "alter materialized view {name}\n  {}",
            refresh_clause(def)
        ));
    }
    if def.rewrite != original.rewrite {
        let verb = if def.rewrite { "enable" } else { "disable" };
        out.push(format!(
            "alter materialized view {name} {verb} query rewrite"
        ));
    }
    Ok(out)
}

/// The form above the query editor (which the document draws).
pub fn ui(ui: &mut Ui, def: &mut MviewDef, original: Option<&MviewDef>, lists: &Lists) {
    let existing = original.is_some();
    form_grid(ui, "mview", |ui| {
        name_field(ui, &mut def.name, existing);
        if !existing {
            form_row(ui, "Tablespace", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut def.tablespace)
                        .hint_text("default")
                        .desired_width(200.0),
                );
                if let Some(ts) = picker(ui, "tablespace", &lists.tablespaces) {
                    def.tablespace = quote_ident(&ts);
                }
            });
            form_row(ui, "Build", |ui| {
                ui.radio_value(&mut def.build_deferred, false, "now")
                    .on_hover_text("BUILD IMMEDIATE: runs the query when created");
                ui.radio_value(&mut def.build_deferred, true, "at the first refresh")
                    .on_hover_text("BUILD DEFERRED");
            });
        }
        form_row(ui, "Refresh", |ui| {
            for (value, label, hint) in [
                (Refresh::Force, "force", "Fast if possible, else complete"),
                (Refresh::Complete, "complete", "Runs the whole query again"),
                (
                    Refresh::Fast,
                    "fast",
                    "Applies the changes only; needs materialized view logs",
                ),
                (Refresh::Never, "never", "Keeps the data as built"),
            ] {
                ui.radio_value(&mut def.refresh, value, label)
                    .on_hover_text(hint);
            }
        });
        if def.refresh != Refresh::Never {
            form_row(ui, "When", |ui| {
                ui.radio_value(&mut def.on_commit, false, "on demand");
                ui.radio_value(&mut def.on_commit, true, "on commit")
                    .on_hover_text("Each commit on the tables refreshes the view");
            });
            if !def.on_commit {
                form_row(ui, "Schedule", |ui| {
                    ui.label("start");
                    ui.add(
                        egui::TextEdit::singleline(&mut def.start)
                            .font(egui::TextStyle::Monospace)
                            .hint_text(if existing { "unchanged" } else { "none" })
                            .desired_width(140.0),
                    );
                    ui.label("next");
                    ui.add(
                        egui::TextEdit::singleline(&mut def.next)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("e.g. sysdate + 1/24")
                            .desired_width(160.0),
                    );
                });
            }
        }
        form_row(ui, "Options", |ui| {
            ui.checkbox(&mut def.rewrite, "query rewrite")
                .on_hover_text("Lets the optimizer answer queries from this view");
        });
    });
    if existing {
        ui.weak("Changing the query drops and recreates the materialized view.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_alters() {
        let mut def = MviewDef::new();
        def.name = "sales_mv".into();
        def.query = "select region, sum(amount) total from sales group by region;".into();
        def.refresh = Refresh::Complete;
        def.start = "sysdate".into();
        def.next = "trunc(sysdate) + 1".into();
        def.rewrite = true;
        assert_eq!(
            statements("APP", None, &def).unwrap(),
            [
                "create materialized view APP.SALES_MV\n  build immediate\n  \
                 refresh complete on demand start with sysdate next trunc(sysdate) + 1\n  \
                 enable query rewrite\nas\nselect region, sum(amount) total from sales group by region"
            ]
        );
        def.on_commit = true;
        assert!(statements("APP", None, &def).is_err());

        let original = MviewDef {
            name: "SALES_MV".into(),
            query: "select 1 x from dual".into(),
            ..MviewDef::new()
        };
        let mut edited = original.clone();
        assert!(
            statements("APP", Some(&original), &edited)
                .unwrap()
                .is_empty()
        );
        edited.refresh = Refresh::Never;
        edited.rewrite = true;
        assert_eq!(
            statements("APP", Some(&original), &edited).unwrap(),
            [
                "alter materialized view APP.SALES_MV\n  never refresh",
                "alter materialized view APP.SALES_MV enable query rewrite",
            ]
        );
        edited.query = "select 2 x from dual".into();
        let s = statements("APP", Some(&original), &edited).unwrap();
        assert_eq!(
            s[0],
            "-- The new query is checked first\nselect 1 from (\nselect 2 x from dual\n) where 1 = 0"
        );
        assert_eq!(s[1], "drop materialized view APP.SALES_MV");
        assert!(s[2].starts_with("create materialized view APP.SALES_MV"));
    }
}
