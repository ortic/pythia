//! Trigger: table or view, timing, events, row or statement level, `WHEN`,
//! or a compound trigger. Becomes the trigger's first source, written
//! further (and compiled) in a program window.

use egui::{RichText, Ui};

use super::program::source_name;
use super::{Lists, form_grid, form_row, ident, picker};
use crate::documents::quote_ident;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Timing {
    #[default]
    Before,
    After,
    InsteadOf,
}

impl Timing {
    fn keyword(self) -> &'static str {
        match self {
            Timing::Before => "before",
            Timing::After => "after",
            Timing::InsteadOf => "instead of",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TriggerDef {
    /// Empty: named after the table and events (see [`TriggerDef::suggested_name`]).
    pub name: String,
    pub table: String,
    /// The table is a view: the trigger is `INSTEAD OF`.
    pub on_view: bool,
    pub timing: Timing,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    /// `UPDATE OF` these columns (comma separated); empty: any.
    pub update_columns: String,
    pub row_level: bool,
    /// `WHEN` condition of a row trigger (`new.status = 'X'`).
    pub when: String,
    /// A compound trigger with a section per timing point.
    pub compound: bool,
    pub enabled: bool,
}

impl Default for TriggerDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            table: String::new(),
            on_view: false,
            timing: Timing::Before,
            insert: true,
            update: true,
            delete: false,
            update_columns: String::new(),
            row_level: true,
            when: String::new(),
            compound: false,
            enabled: true,
        }
    }
}

impl TriggerDef {
    /// A trigger on a table, or `INSTEAD OF` on a view.
    pub fn on(table: &str, view: bool) -> Self {
        Self {
            table: quote_ident(table),
            on_view: view,
            timing: if view {
                Timing::InsteadOf
            } else {
                Timing::Before
            },
            ..Self::default()
        }
    }

    /// `ORDERS_BIU`: table, timing and events.
    pub fn suggested_name(&self) -> String {
        let table = ident(&self.table);
        if table.is_empty() {
            return String::new();
        }
        let timing = if self.compound {
            "C"
        } else {
            match self.timing {
                Timing::Before => "B",
                Timing::After => "A",
                Timing::InsteadOf => "IO",
            }
        };
        let events: String = [(self.insert, 'I'), (self.update, 'U'), (self.delete, 'D')]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, c)| *c)
            .collect();
        let tail = format!("_{timing}{events}");
        let mut head = table;
        while head.len() + tail.len() > 30 {
            head.pop();
        }
        format!("{head}{tail}")
    }

    /// The name as typed, else the suggested one.
    pub fn effective_name(&self) -> String {
        match self.name.trim() {
            "" => quote_ident(&self.suggested_name()),
            n => n.to_owned(),
        }
    }
}

fn validate(def: &TriggerDef) -> Result<(), String> {
    if def.table.trim().is_empty() {
        return Err(if def.on_view {
            "Choose the view".into()
        } else {
            "Choose the table".into()
        });
    }
    if !(def.insert || def.update || def.delete) {
        return Err("Choose at least one of insert, update and delete".into());
    }
    if def.on_view && !def.compound && def.timing != Timing::InsteadOf {
        return Err("A trigger on a view is INSTEAD OF".into());
    }
    if !def.on_view && def.timing == Timing::InsteadOf {
        return Err("INSTEAD OF triggers are for views".into());
    }
    if def.on_view && def.update && !def.update_columns.trim().is_empty() {
        return Err("A trigger on a view fires on any update, not of columns".into());
    }
    if !def.when.trim().is_empty() && (!def.row_level || def.on_view || def.compound) {
        return Err("Only a row trigger on a table takes a WHEN condition".into());
    }
    Ok(())
}

/// `insert or update of a, b or delete`
fn events(def: &TriggerDef) -> String {
    let mut out = Vec::new();
    if def.insert {
        out.push("insert".to_owned());
    }
    if def.update {
        match def.update_columns.trim() {
            "" => out.push("update".to_owned()),
            columns => out.push(format!("update of {columns}")),
        }
    }
    if def.delete {
        out.push("delete".to_owned());
    }
    out.join(" or ")
}

/// A body that branches on the firing event when there are several.
fn body(def: &TriggerDef, indent: &str) -> String {
    let fired: Vec<&str> = [
        (def.insert, "inserting"),
        (def.update, "updating"),
        (def.delete, "deleting"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, p)| *p)
    .collect();
    if fired.len() < 2 {
        return format!("{indent}begin\n{indent}  null;\n{indent}end");
    }
    let mut text = format!("{indent}begin\n");
    for (i, predicate) in fired.iter().enumerate() {
        let keyword = if i == 0 { "if" } else { "elsif" };
        text.push_str(&format!(
            "{indent}  {keyword} {predicate} then\n{indent}    null;\n"
        ));
    }
    text.push_str(&format!("{indent}  end if;\n{indent}end"));
    text
}

pub fn source(owner: &str, def: &TriggerDef) -> Result<String, String> {
    validate(def)?;
    let name = source_name(owner, &def.effective_name());
    let table = source_name(owner, &def.table);
    let disable = if def.enabled { "" } else { "\n  disable" };
    if def.compound {
        let points: &[&str] = if def.on_view {
            &["instead of each row"]
        } else {
            &[
                "before statement",
                "before each row",
                "after each row",
                "after statement",
            ]
        };
        let sections: Vec<String> = points
            .iter()
            .map(|p| format!("  {p} is\n  begin\n    null;\n  end {p};"))
            .collect();
        let end = name.rsplit('.').next().unwrap_or(&name).to_owned();
        return Ok(format!(
            "create or replace trigger {name}\n  for {} on {table}{disable}\n  compound trigger\n\n{}\nend {end};\n",
            events(def),
            sections.join("\n\n")
        ));
    }
    let mut text = format!(
        "create or replace trigger {name}\n  {} {} on {table}",
        def.timing.keyword(),
        events(def)
    );
    if def.row_level || def.on_view {
        text.push_str("\n  for each row");
    }
    text.push_str(disable);
    if !def.when.trim().is_empty() {
        // A `--` comment at its end would take the `)` with it.
        let when = def.when.trim();
        let end = if when.contains("--") { "\n  " } else { "" };
        text.push_str(&format!("\n  when ({when}{end})"));
    }
    text.push('\n');
    text.push_str(&body(def, ""));
    text.push_str(";\n");
    Ok(text)
}

pub fn ui(ui: &mut Ui, def: &mut TriggerDef, lists: &mut Lists) {
    form_grid(ui, "trigger", |ui| {
        form_row(ui, "On", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut def.table)
                    .hint_text("table or view")
                    .desired_width(240.0),
            );
            let mut targets = lists.tables.clone();
            targets.extend(lists.views.iter().cloned());
            targets.sort();
            if let Some(t) = picker(ui, "table", &targets) {
                def.on_view = lists.views.contains(&t);
                def.table = quote_ident(&t);
                if def.on_view {
                    def.timing = Timing::InsteadOf;
                    def.when.clear();
                    def.update_columns.clear();
                } else if def.timing == Timing::InsteadOf {
                    def.timing = Timing::Before;
                }
            }
            if def.on_view {
                ui.weak("view");
            }
        });
        form_row(ui, "Name", |ui| {
            let suggested = def.suggested_name().to_lowercase();
            ui.add(
                egui::TextEdit::singleline(&mut def.name)
                    .hint_text(if suggested.is_empty() {
                        "name".to_owned()
                    } else {
                        suggested
                    })
                    .desired_width(240.0),
            );
        });
        form_row(ui, "Kind", |ui| {
            ui.radio_value(&mut def.compound, false, "simple");
            ui.radio_value(&mut def.compound, true, "compound")
                .on_hover_text("One trigger with a section per timing point, sharing state");
        });
        if !def.compound {
            form_row(ui, "Fires", |ui| {
                ui.add_enabled_ui(!def.on_view, |ui| {
                    ui.radio_value(&mut def.timing, Timing::Before, "before");
                    ui.radio_value(&mut def.timing, Timing::After, "after");
                });
                ui.add_enabled_ui(def.on_view, |ui| {
                    ui.radio_value(&mut def.timing, Timing::InsteadOf, "instead of")
                        .on_disabled_hover_text("For views");
                });
            });
        }
        form_row(ui, "Events", |ui| {
            ui.checkbox(&mut def.insert, "insert");
            ui.checkbox(&mut def.update, "update");
            ui.checkbox(&mut def.delete, "delete");
        });
        if def.update && !def.on_view {
            form_row(ui, "Update of", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut def.update_columns)
                        .hint_text("any column")
                        .desired_width(240.0),
                );
                let table = ident(&def.table);
                if !table.is_empty() {
                    match lists.columns.get(&table) {
                        Some(columns) => {
                            if let Some(c) = picker(ui, "column", columns) {
                                if !def.update_columns.trim().is_empty() {
                                    def.update_columns.push_str(", ");
                                }
                                def.update_columns.push_str(&quote_ident(&c).to_lowercase());
                            }
                        }
                        None => lists.want_columns = Some(table),
                    }
                }
            });
        }
        if !def.compound && !def.on_view {
            form_row(ui, "Level", |ui| {
                ui.radio_value(&mut def.row_level, true, "each row")
                    .on_hover_text("Fires once per row, with :new and :old");
                ui.radio_value(&mut def.row_level, false, "statement")
                    .on_hover_text("Fires once per statement");
            });
            if def.row_level {
                form_row(ui, "When", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut def.when)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("always, or e.g. new.status = 'OPEN' (no colon)")
                            .desired_width(360.0),
                    );
                });
            }
        }
        form_row(ui, "State", |ui| {
            ui.checkbox(&mut def.enabled, "enabled");
        });
    });
    ui.add_space(6.0);
    ui.label(
        RichText::new("Open in editor writes the trigger's body; compiling creates it.")
            .small()
            .weak(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_triggers() {
        let mut def = TriggerDef::on("ORDERS", false);
        assert_eq!(def.suggested_name(), "ORDERS_BIU");
        def.update_columns = "status, total".into();
        def.when = "new.status <> old.status".into();
        assert_eq!(
            source("HR", &def).unwrap(),
            "create or replace trigger hr.orders_biu\n  before insert or update of status, total on hr.orders\n  \
             for each row\n  when (new.status <> old.status)\nbegin\n  if inserting then\n    null;\n  \
             elsif updating then\n    null;\n  end if;\nend;\n"
        );
        // A `--` comment doesn't take the `)` with it.
        def.when = "new.status <> old.status -- changed".into();
        assert!(
            source("HR", &def)
                .unwrap()
                .contains("\n  when (new.status <> old.status -- changed\n  )\nbegin"),
        );
        def.when.clear();
        def.update = false;
        def.row_level = false;
        def.timing = Timing::After;
        def.enabled = false;
        def.name = "audit_orders".into();
        assert_eq!(
            source("HR", &def).unwrap(),
            "create or replace trigger hr.audit_orders\n  after insert on hr.orders\n  disable\nbegin\n  null;\nend;\n"
        );
        def.when = "1 = 1".into();
        assert!(source("HR", &def).is_err(), "WHEN needs a row trigger");
        def.when.clear();
        def.timing = Timing::InsteadOf;
        assert!(source("HR", &def).is_err());
    }

    #[test]
    fn view_and_compound_triggers() {
        let mut def = TriggerDef::on("ORDERS_V", true);
        def.delete = true;
        def.update = false;
        assert_eq!(def.suggested_name(), "ORDERS_V_IOID");
        assert_eq!(
            source("HR", &def).unwrap(),
            "create or replace trigger hr.orders_v_ioid\n  instead of insert or delete on hr.orders_v\n  \
             for each row\nbegin\n  if inserting then\n    null;\n  elsif deleting then\n    null;\n  \
             end if;\nend;\n"
        );
        let mut def = TriggerDef::on("ORDERS", false);
        def.compound = true;
        def.insert = false;
        let s = source("HR", &def).unwrap();
        assert!(
            s.starts_with("create or replace trigger hr.orders_cu\n  for update on hr.orders\n  compound trigger\n\n  before statement is\n"),
            "{s}"
        );
        assert!(
            s.ends_with("  end after statement;\nend orders_cu;\n"),
            "{s}"
        );
    }
}
