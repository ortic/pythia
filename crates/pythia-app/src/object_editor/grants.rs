//! Grants on an object: a grantee × privilege matrix; each cell cycles
//! through none, granted and granted with grant option. Changes become
//! `GRANT` / `REVOKE` statements.

use std::collections::BTreeSet;

use egui::{Color32, RichText, Ui};
use pythia_db::oracle::Connection;
use pythia_db::{DbError, Dialect, ObjectType};

use super::{Lists, ident, picker};
use crate::documents::quote_ident;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GrantRow {
    pub grantee: String,
    pub granted: BTreeSet<String>,
    /// Granted `WITH GRANT OPTION` (a subset of `granted`).
    pub grantable: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GrantsDef {
    pub rows: Vec<GrantRow>,
    /// Privileges shown as columns.
    pub privileges: Vec<String>,
    /// Grantee being added.
    pub new_grantee: String,
}

/// The privileges that make sense for an object type.
fn privileges_for(t: ObjectType) -> &'static [&'static str] {
    match t {
        ObjectType::Table => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "REFERENCES",
            "ALTER",
            "INDEX",
            "DEBUG",
        ],
        ObjectType::View => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "REFERENCES",
            "DEBUG",
        ],
        ObjectType::MaterializedView => &["SELECT"],
        ObjectType::Sequence => &["SELECT", "ALTER"],
        ObjectType::Package
        | ObjectType::Procedure
        | ObjectType::Function
        | ObjectType::Type
        | ObjectType::Library => &["EXECUTE", "DEBUG"],
        // EXECUTE: preprocessor programs of external tables.
        ObjectType::Directory => &["READ", "WRITE", "EXECUTE"],
        _ => &["SELECT", "EXECUTE"],
    }
}

pub fn load(
    conn: &Connection,
    owner: &str,
    name: &str,
    object_type: ObjectType,
) -> Result<GrantsDef, DbError> {
    // Directories belong to SYS.
    let owner = match object_type {
        ObjectType::Directory => "SYS",
        _ => owner,
    };
    let mut def = GrantsDef {
        privileges: privileges_for(object_type)
            .iter()
            .map(|p| (*p).to_owned())
            .collect(),
        ..GrantsDef::default()
    };
    // ALL_TAB_PRIVS leaves out grants the session takes no part in, which
    // a DBA looking at another schema's object wants to see.
    let read = |sql: &str| -> Result<Vec<(String, String, String)>, DbError> {
        let mut out = Vec::new();
        for row in conn.query_as::<(String, String, String)>(sql, &[&owner, &name])? {
            out.push(row?);
        }
        Ok(out)
    };
    let rows = read(
        "select grantee, privilege, grantable from dba_tab_privs
          where owner = :1 and table_name = :2
          order by grantee, privilege",
    )
    .or_else(|_| {
        read(
            "select grantee, privilege, grantable from all_tab_privs
              where table_schema = :1 and table_name = :2
              order by grantee, privilege",
        )
    })?;
    for (grantee, privilege, grantable) in rows {
        if !def.privileges.contains(&privilege) {
            def.privileges.push(privilege.clone());
        }
        let row = match def.rows.iter_mut().find(|r| r.grantee == grantee) {
            Some(r) => r,
            None => {
                def.rows.push(GrantRow {
                    grantee,
                    ..GrantRow::default()
                });
                def.rows.last_mut().expect("just pushed")
            }
        };
        row.granted.insert(privilege.clone());
        if grantable == "YES" {
            row.grantable.insert(privilege);
        }
    }
    Ok(def)
}

pub fn statements(object: &str, original: &GrantsDef, def: &GrantsDef) -> Vec<String> {
    let empty = GrantRow::default();
    let mut grantees: Vec<&str> = def.rows.iter().map(|r| r.grantee.as_str()).collect();
    for r in &original.rows {
        if !grantees.contains(&r.grantee.as_str()) {
            grantees.push(&r.grantee);
        }
    }
    let list = |set: &BTreeSet<String>| {
        set.iter()
            .map(|p| p.to_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = Vec::new();
    for grantee in grantees {
        let before = original
            .rows
            .iter()
            .find(|r| r.grantee == grantee)
            .unwrap_or(&empty);
        let after = def
            .rows
            .iter()
            .find(|r| r.grantee == grantee)
            .unwrap_or(&empty);
        // Losing the grant option means revoking and granting again.
        let downgraded: BTreeSet<String> = before
            .grantable
            .iter()
            .filter(|p| after.granted.contains(*p) && !after.grantable.contains(*p))
            .cloned()
            .collect();
        let revoke: BTreeSet<String> = before
            .granted
            .difference(&after.granted)
            .cloned()
            .chain(downgraded.iter().cloned())
            .collect();
        let grant: BTreeSet<String> = after
            .granted
            .iter()
            .filter(|p| !after.grantable.contains(*p))
            .filter(|p| !before.granted.contains(*p) || downgraded.contains(*p))
            .cloned()
            .collect();
        let grant_option: BTreeSet<String> = after
            .grantable
            .difference(&before.grantable)
            .cloned()
            .collect();
        let who = quote_ident(grantee);
        if !revoke.is_empty() {
            out.push(format!("revoke {} on {object} from {who}", list(&revoke)));
        }
        if !grant.is_empty() {
            out.push(format!("grant {} on {object} to {who}", list(&grant)));
        }
        if !grant_option.is_empty() {
            out.push(format!(
                "grant {} on {object} to {who} with grant option",
                list(&grant_option)
            ));
        }
    }
    out
}

pub fn ui(ui: &mut Ui, def: &mut GrantsDef, lists: &Lists) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut def.new_grantee)
                .hint_text("user or role")
                .desired_width(200.0),
        );
        if let Some(g) = picker(ui, "grantee", &lists.grantees) {
            def.new_grantee = g;
        }
        let grantee = ident(&def.new_grantee);
        let known = def.rows.iter().any(|r| r.grantee == grantee);
        if ui
            .add_enabled(
                !grantee.is_empty() && !known,
                egui::Button::new("Add grantee"),
            )
            .clicked()
        {
            def.rows.push(GrantRow {
                grantee,
                ..GrantRow::default()
            });
            def.new_grantee.clear();
        }
        ui.weak("Click a cell: granted → with grant option → none");
    });
    ui.add_space(6.0);
    if def.rows.is_empty() {
        ui.weak("No grants on this object yet.");
        return;
    }
    let mut remove = None;
    egui::ScrollArea::horizontal().show(ui, |ui| {
        egui::Grid::new("grants")
            .num_columns(def.privileges.len() + 2)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                ui.strong("Grantee");
                for p in &def.privileges {
                    ui.strong(p.to_lowercase());
                }
                ui.label("");
                ui.end_row();
                for (i, row) in def.rows.iter_mut().enumerate() {
                    // As the object browser shows users and roles (#272).
                    let grantee = crate::browser::shown_name(Dialect::ORACLE, &row.grantee);
                    ui.label(RichText::new(grantee).monospace());
                    for p in &def.privileges {
                        let state = if row.grantable.contains(p) {
                            Cell::GrantOption
                        } else if row.granted.contains(p) {
                            Cell::Granted
                        } else {
                            Cell::None
                        };
                        let cell = cell_ui(ui, state).on_hover_text(format!(
                            "{} {}",
                            p.to_lowercase(),
                            state.label()
                        ));
                        if cell.clicked() {
                            if row.grantable.remove(p) {
                                row.granted.remove(p);
                            } else if row.granted.contains(p) {
                                row.grantable.insert(p.clone());
                            } else {
                                row.granted.insert(p.clone());
                            }
                        }
                    }
                    if ui
                        .small_button("×")
                        .on_hover_text("Revoke everything")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
    });
    if let Some(i) = remove {
        def.rows.remove(i);
    }
}

#[derive(Clone, Copy)]
enum Cell {
    None,
    Granted,
    GrantOption,
}

impl Cell {
    fn label(self) -> &'static str {
        match self {
            Cell::None => "not granted",
            Cell::Granted => "granted",
            Cell::GrantOption => "granted with grant option",
        }
    }
}

/// A clickable matrix cell: a check mark, with a plus for the grant option.
fn cell_ui(ui: &mut Ui, state: Cell) -> egui::Response {
    let size = egui::vec2(40.0, ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = ui.style().interact(&response);
    if response.hovered() {
        ui.painter().rect_filled(rect, 3.0, visuals.bg_fill);
    }
    let c = rect.center() - egui::vec2(4.0, 0.0);
    match state {
        Cell::None => {
            ui.painter()
                .circle_filled(rect.center(), 1.5, ui.visuals().weak_text_color());
        }
        Cell::Granted | Cell::GrantOption => {
            let color = Color32::from_rgb(0x2f, 0xa0, 0x4c);
            let stroke = egui::Stroke::new(2.0, color);
            ui.painter().line(
                vec![
                    c + egui::vec2(-5.0, 0.0),
                    c + egui::vec2(-1.5, 3.5),
                    c + egui::vec2(5.0, -4.0),
                ],
                stroke,
            );
            if matches!(state, Cell::GrantOption) {
                let p = c + egui::vec2(11.0, -3.0);
                let s = egui::Stroke::new(1.5, color);
                ui.painter()
                    .line_segment([p - egui::vec2(3.0, 0.0), p + egui::vec2(3.0, 0.0)], s);
                ui.painter()
                    .line_segment([p - egui::vec2(0.0, 3.0), p + egui::vec2(0.0, 3.0)], s);
            }
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(grantee: &str, granted: &[&str], grantable: &[&str]) -> GrantRow {
        GrantRow {
            grantee: grantee.into(),
            granted: granted.iter().map(|p| (*p).to_owned()).collect(),
            grantable: grantable.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn diffs_grants() {
        let original = GrantsDef {
            rows: vec![
                row("REPORTING", &["SELECT"], &["SELECT"]),
                row("APP_USER", &["SELECT", "INSERT"], &[]),
            ],
            ..GrantsDef::default()
        };
        let edited = GrantsDef {
            rows: vec![
                row("REPORTING", &["SELECT"], &[]),
                row("AUDITOR", &["SELECT"], &["SELECT"]),
            ],
            ..GrantsDef::default()
        };
        assert_eq!(
            statements("HR.EMP", &original, &edited),
            [
                "revoke select on HR.EMP from REPORTING",
                "grant select on HR.EMP to REPORTING",
                "grant select on HR.EMP to AUDITOR with grant option",
                "revoke insert, select on HR.EMP from APP_USER",
            ]
        );
        assert!(statements("HR.EMP", &original, &original).is_empty());
    }
}
