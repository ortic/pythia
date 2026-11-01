//! Partitions in the table editor: a new table's partitioning (method,
//! key, partitions), and an existing table's partitions with partitions
//! added, truncated or dropped.

use std::collections::BTreeMap;

use egui::{RichText, Ui};
use pythia_compare::script::{Phase, SyncStatement};
use pythia_compare::table::{self, PartitionDef, PartitionMethod, Partitioning};
use pythia_db::oracle::Connection;
use pythia_db::{DbError, Dialect};

use crate::documents::quote_ident;
use crate::object_editor::{ident, picker};

/// What happens to an existing partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Delete its rows.
    Truncate,
    Drop,
}

/// An existing table's partitioning as loaded, and the changes to it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Existing {
    /// `RANGE`, `LIST`, `HASH`, `REFERENCE`, `SYSTEM`; with `INTERVAL`.
    pub kind: String,
    /// The method, if it's one the editor handles.
    pub method: Option<PartitionMethod>,
    pub columns: Vec<String>,
    pub interval: String,
    /// Partitions in order, names as in the dictionary, and whether the
    /// database created it for an interval.
    pub partitions: Vec<(PartitionDef, bool)>,
    /// Partitions to add (names as typed).
    pub added: Vec<PartitionDef>,
    /// Partitions to truncate or drop, by name.
    pub actions: BTreeMap<String, Action>,
}

impl Existing {
    pub fn is_changed(&self) -> bool {
        !self.added.is_empty() || !self.actions.is_empty()
    }

    /// Why no partition can be added, if none can: a catch-all partition
    /// takes everything a new one would (it would have to be split).
    pub fn add_blocked(&self) -> Option<String> {
        let (catch_all, bound) = match self.method {
            Some(PartitionMethod::Range) => ("MAXVALUE", "MAXVALUE"),
            Some(PartitionMethod::List) => ("DEFAULT", "DEFAULT"),
            _ => return None,
        };
        // A catch-all being dropped no longer blocks.
        let (last, _) = self
            .partitions
            .iter()
            .rfind(|(p, _)| self.actions.get(&p.name) != Some(&Action::Drop))?;
        last.bound.trim().eq_ignore_ascii_case(bound).then(|| {
            format!(
                "Partition {} ({catch_all}) takes the values a new one would: it has to be split instead (not offered here), or dropped",
                shown(&last.name)
            )
        })
    }

    /// The partitioning to start a copy of the table with (names as
    /// typed), without the partitions an interval created.
    pub fn copy(&self) -> Option<Partitioning> {
        let method = self.method?;
        Some(Partitioning {
            method,
            columns: self.columns.iter().map(|c| quote_ident(c)).collect(),
            interval: self.interval.clone(),
            partitions: self
                .partitions
                .iter()
                .filter(|(_, interval)| !interval)
                .map(|(p, _)| PartitionDef {
                    name: quote_ident(&p.name),
                    tablespace: quote_ident(&p.tablespace),
                    ..p.clone()
                })
                .collect(),
            hash_count: self.partitions.len() as u32,
        })
    }
}

/// The partitioning of `owner.table`, if it's partitioned.
pub fn load(conn: &Connection, owner: &str, table: &str) -> Result<Option<Existing>, DbError> {
    let Some(row) = conn
        .query_as::<(String, Option<String>)>(
            "select partitioning_type, interval from all_part_tables
              where owner = :1 and table_name = :2",
            &[&owner, &table],
        )?
        .next()
    else {
        return Ok(None);
    };
    let (kind, interval) = row?;
    let interval = interval.unwrap_or_default();
    let method = match kind.as_str() {
        "RANGE" if !interval.is_empty() => Some(PartitionMethod::Interval),
        "RANGE" => Some(PartitionMethod::Range),
        "LIST" => Some(PartitionMethod::List),
        "HASH" => Some(PartitionMethod::Hash),
        _ => None,
    };
    let mut columns = Vec::new();
    for row in conn.query_as::<String>(
        "select column_name from all_part_key_columns
          where owner = :1 and name = :2 and object_type = 'TABLE'
          order by column_position",
        &[&owner, &table],
    )? {
        columns.push(row?);
    }
    let mut partitions = Vec::new();
    // HIGH_VALUE is a LONG.
    for row in conn.query_as::<(String, Option<String>, Option<String>, Option<String>)>(
        "select partition_name, high_value, tablespace_name, interval
           from all_tab_partitions where table_owner = :1 and table_name = :2
          order by partition_position",
        &[&owner, &table],
    )? {
        let (name, bound, tablespace, by_interval) = row?;
        partitions.push((
            PartitionDef {
                name,
                bound: bound.unwrap_or_default(),
                tablespace: tablespace.unwrap_or_default(),
            },
            by_interval.as_deref() == Some("YES"),
        ));
    }
    let kind = if interval.is_empty() {
        kind
    } else {
        format!("{kind} INTERVAL")
    };
    Ok(Some(Existing {
        kind,
        method,
        columns,
        interval,
        partitions,
        ..Existing::default()
    }))
}

/// A new table's partitioning with the names as the dictionary has them;
/// `key` is the typed key column list.
pub fn to_dictionary(p: &Partitioning, key: &str) -> Partitioning {
    Partitioning {
        columns: split(key).iter().map(|c| ident(c)).collect(),
        partitions: p
            .partitions
            .iter()
            .map(|d| PartitionDef {
                name: ident(&d.name),
                bound: d.bound.trim().to_owned(),
                tablespace: ident(&d.tablespace),
            })
            .collect(),
        ..p.clone()
    }
}

fn split(text: &str) -> Vec<String> {
    text.split(',')
        .map(|c| c.trim().to_owned())
        .filter(|c| !c.is_empty())
        .collect()
}

/// What keeps a new table's partitioning from becoming SQL. `columns` are
/// the table's (dictionary names).
pub fn validate_new(
    p: &Partitioning,
    key: &str,
    columns: &[String],
    temporary: bool,
) -> Result<(), String> {
    if temporary {
        return Err("A temporary table can't be partitioned".into());
    }
    let key = split(key);
    if key.is_empty() {
        return Err("Choose the partition key".into());
    }
    if let Some(missing) = key.iter().find(|k| !columns.contains(&ident(k))) {
        return Err(format!(
            "The partition key uses {missing}, which isn't a column"
        ));
    }
    if p.method == PartitionMethod::Interval {
        if key.len() > 1 {
            return Err("An interval partition key is one column".into());
        }
        if p.interval.trim().is_empty() {
            return Err("Enter the interval".into());
        }
    }
    if p.method != PartitionMethod::Hash && p.partitions.is_empty() {
        return Err("Add a partition".into());
    }
    let mut seen = std::collections::HashSet::new();
    for (i, part) in p.partitions.iter().enumerate() {
        if p.method != PartitionMethod::Hash && part.bound.trim().is_empty() {
            return Err(format!("Partition {} has no bound", i + 1));
        }
        let name = ident(&part.name);
        if !name.is_empty() && !seen.insert(name.clone()) {
            return Err(format!("Partition {name} appears twice"));
        }
    }
    Ok(())
}

/// The statements for an existing table's partition changes. Truncating
/// and dropping lose rows: enabled only if `drops`.
pub fn statements(
    table_name: &str,
    existing: &Existing,
    drops: bool,
) -> Result<Vec<SyncStatement>, String> {
    let t = table::ident(table_name);
    let mut out = Vec::new();
    for (name, action) in &existing.actions {
        let p = table::ident(name);
        let (sql, warning) = match action {
            Action::Truncate => (
                format!("alter table {t} truncate partition {p}"),
                format!("deletes the rows of partition {name}"),
            ),
            Action::Drop => (
                format!("alter table {t} drop partition {p}"),
                format!("drops partition {name} and its rows"),
            ),
        };
        out.push(SyncStatement {
            phase: Phase::Tables,
            sql,
            plsql: false,
            warning: Some(warning),
            enabled: drops,
            object: table_name.to_owned(),
        });
    }
    if !existing.added.is_empty() {
        let method = match existing.method {
            Some(PartitionMethod::Interval) => {
                return Err("An interval table adds its partitions itself".into());
            }
            Some(m) => m,
            None => {
                return Err(format!(
                    "Adding partitions to a {} table isn't supported here",
                    existing.kind.to_lowercase()
                ));
            }
        };
        if let Some(blocked) = existing.add_blocked() {
            return Err(blocked);
        }
        for (i, part) in existing.added.iter().enumerate() {
            if method != PartitionMethod::Hash && part.bound.trim().is_empty() {
                return Err(format!("New partition {} has no bound", i + 1));
            }
            let def = PartitionDef {
                name: ident(&part.name),
                bound: part.bound.trim().to_owned(),
                tablespace: ident(&part.tablespace),
            };
            out.push(SyncStatement::new(
                Phase::Tables,
                table_name,
                format!("alter table {t} add {}", def.definition(method)),
            ));
        }
    }
    Ok(out)
}

/// Interval presets: (label, expression).
const INTERVALS: &[(&str, &str)] = &[
    ("month", "NUMTOYMINTERVAL(1, 'MONTH')"),
    ("year", "NUMTOYMINTERVAL(1, 'YEAR')"),
    ("day", "NUMTODSINTERVAL(1, 'DAY')"),
    ("1000 (numbers)", "1000"),
];

fn bound_hint(method: PartitionMethod) -> &'static str {
    match method {
        PartitionMethod::List => "values, e.g. 'A', 'B'",
        PartitionMethod::Hash => "",
        _ => "less than, e.g. DATE '2027-01-01'",
    }
}

/// The partitions grid: name, bound, tablespace; returns nothing, edits in
/// place.
fn partitions_grid(
    ui: &mut Ui,
    id: &str,
    method: PartitionMethod,
    partitions: &mut Vec<PartitionDef>,
    tablespaces: &[String],
) {
    let mut remove = None;
    let mut swap = None;
    let n = partitions.len();
    let height = ui.spacing().interact_size.y;
    egui::Grid::new(id)
        .num_columns(5)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.strong("Name");
            ui.strong(match method {
                PartitionMethod::List => "Values",
                PartitionMethod::Hash => "",
                _ => "Values less than",
            });
            ui.strong("Tablespace");
            ui.end_row();
            for (i, p) in partitions.iter_mut().enumerate() {
                ui.add_sized(
                    [150.0, height],
                    egui::TextEdit::singleline(&mut p.name)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("automatic"),
                );
                if method == PartitionMethod::Hash {
                    ui.label("");
                } else {
                    ui.add_sized(
                        [260.0, height],
                        egui::TextEdit::singleline(&mut p.bound)
                            .font(egui::TextStyle::Monospace)
                            .hint_text(bound_hint(method)),
                    );
                }
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [130.0, height],
                        egui::TextEdit::singleline(&mut p.tablespace).hint_text("the table's"),
                    );
                    if let Some(ts) = picker(ui, (id, "tablespace", i), tablespaces) {
                        p.tablespace = quote_ident(&ts);
                    }
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(i > 0, egui::Button::new("↑").small())
                        .clicked()
                    {
                        swap = Some((i - 1, i));
                    }
                    if ui
                        .add_enabled(i + 1 < n, egui::Button::new("↓").small())
                        .clicked()
                    {
                        swap = Some((i, i + 1));
                    }
                });
                if ui.small_button("×").on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some((a, b)) = swap {
        partitions.swap(a, b);
    }
    if let Some(i) = remove {
        partitions.remove(i);
    }
}

/// Buttons adding a partition (and the catch-all one of the method).
fn add_buttons(ui: &mut Ui, method: PartitionMethod, partitions: &mut Vec<PartitionDef>) {
    ui.horizontal(|ui| {
        if ui.button("Add partition").clicked() {
            partitions.push(PartitionDef {
                name: format!("P{}", partitions.len() + 1),
                ..PartitionDef::default()
            });
        }
        let rest = match method {
            PartitionMethod::Range => Some(("Add MAXVALUE partition", "P_MAX", "MAXVALUE")),
            PartitionMethod::List => Some(("Add DEFAULT partition", "P_DEFAULT", "DEFAULT")),
            _ => None,
        };
        if let Some((label, name, bound)) = rest
            && ui
                .button(label)
                .on_hover_text("Takes the rows no other partition takes")
                .clicked()
        {
            partitions.push(PartitionDef {
                name: name.into(),
                bound: bound.into(),
                tablespace: String::new(),
            });
        }
    });
}

/// A new table's partitioning. `columns` are the table's (as typed).
pub fn new_table_ui(
    ui: &mut Ui,
    partitioning: &mut Option<Partitioning>,
    key: &mut String,
    columns: &[String],
    tablespaces: &[String],
) {
    egui::Grid::new("new-partitioning")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label("Partitioned by");
            let current = partitioning.as_ref().map(|p| p.method);
            egui::ComboBox::from_id_salt("partition-method")
                .selected_text(current.map_or("not partitioned", PartitionMethod::label))
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(current.is_none(), "not partitioned")
                        .clicked()
                    {
                        *partitioning = None;
                    }
                    for (method, hint) in [
                        (
                            PartitionMethod::Range,
                            "Ranges of the key, e.g. one per year",
                        ),
                        (
                            PartitionMethod::Interval,
                            "Ranges the database adds as rows arrive, e.g. monthly",
                        ),
                        (
                            PartitionMethod::List,
                            "Lists of key values, e.g. per region",
                        ),
                        (PartitionMethod::Hash, "Spreads rows evenly by the key"),
                    ] {
                        if ui
                            .selectable_label(current == Some(method), method.label())
                            .on_hover_text(hint)
                            .clicked()
                        {
                            let p = partitioning.get_or_insert_with(|| Partitioning {
                                hash_count: 4,
                                ..Partitioning::default()
                            });
                            p.method = method;
                            if method == PartitionMethod::Interval && p.interval.is_empty() {
                                p.interval = INTERVALS[0].1.into();
                            }
                        }
                    }
                });
            ui.end_row();
            let Some(p) = partitioning.as_mut() else {
                return;
            };
            ui.label("Key");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(key)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("columns")
                        .desired_width(240.0),
                );
                if let Some(c) = picker(ui, "partition-key", columns) {
                    if !key.trim().is_empty() {
                        key.push_str(", ");
                    }
                    key.push_str(&c);
                }
            });
            ui.end_row();
            if p.method == PartitionMethod::Interval {
                ui.label("Interval");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut p.interval)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(240.0),
                    );
                    let labels: Vec<String> =
                        INTERVALS.iter().map(|(l, _)| (*l).to_owned()).collect();
                    if let Some(l) = picker(ui, "interval", &labels)
                        && let Some((_, expr)) = INTERVALS.iter().find(|(label, _)| *label == l)
                    {
                        p.interval = (*expr).into();
                    }
                });
                ui.end_row();
            }
            if p.method == PartitionMethod::Hash && p.partitions.is_empty() {
                ui.label("Partitions");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut p.hash_count).range(1..=1024));
                    ui.weak("named by the database, or add them below");
                });
                ui.end_row();
            }
        });
    let Some(p) = partitioning.as_mut() else {
        ui.add_space(6.0);
        ui.weak(
            "Partitioning needs the Partitioning option (Enterprise Edition, or Oracle Free 23ai).",
        );
        return;
    };
    ui.add_space(8.0);
    if p.method == PartitionMethod::Interval {
        ui.weak("The first range partitions; the database adds one per interval above them.");
    }
    let method = p.method;
    partitions_grid(ui, "new-partitions", method, &mut p.partitions, tablespaces);
    add_buttons(ui, method, &mut p.partitions);
}

/// A name as the object browser shows it (#272).
fn shown(name: &str) -> String {
    crate::browser::shown_name(Dialect::ORACLE, name)
}

/// An existing table's partitions: their bounds, and partitions to add,
/// truncate or drop.
pub fn existing_ui(ui: &mut Ui, existing: &mut Existing, tablespaces: &[String]) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("Partitioned by {}", existing.kind.to_lowercase())).strong(),
        );
        let columns: Vec<String> = existing.columns.iter().map(|c| shown(c)).collect();
        ui.label(RichText::new(format!("({})", columns.join(", "))).monospace());
        if !existing.interval.is_empty() {
            ui.label(RichText::new(format!("interval {}", existing.interval)).monospace());
        }
    });
    ui.add_space(6.0);
    let mut toggles = Vec::new();
    egui::ScrollArea::vertical()
        .max_height(ui.available_height() * 0.6)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("existing-partitions")
                .striped(true)
                .num_columns(5)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for title in ["Name", "Bound", "Tablespace", "", ""] {
                        ui.strong(title);
                    }
                    ui.end_row();
                    for (p, by_interval) in &existing.partitions {
                        let action = existing.actions.get(&p.name).copied();
                        let mut name = RichText::new(shown(&p.name)).monospace();
                        if action == Some(Action::Drop) {
                            name = name.strikethrough();
                        }
                        ui.label(name);
                        ui.label(RichText::new(&p.bound).monospace());
                        ui.label(shown(&p.tablespace));
                        if *by_interval {
                            ui.weak("added by the interval");
                        } else {
                            ui.label("");
                        }
                        ui.horizontal(|ui| {
                            for (a, label, hint) in [
                                (Action::Truncate, "truncate", "Delete the partition's rows"),
                                (Action::Drop, "drop", "Drop the partition and its rows"),
                            ] {
                                if ui
                                    .selectable_label(action == Some(a), label)
                                    .on_hover_text(hint)
                                    .clicked()
                                {
                                    toggles.push((p.name.clone(), a));
                                }
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    for (name, action) in toggles {
        if existing.actions.get(&name) == Some(&action) {
            existing.actions.remove(&name);
        } else {
            existing.actions.insert(name, action);
        }
    }
    if !existing.actions.is_empty() {
        ui.weak("Truncating and dropping run only with \"Allow dropping\" on the Script tab.");
    }
    ui.add_space(8.0);
    match existing.method {
        Some(PartitionMethod::Interval) => {
            ui.weak("The database adds an interval table's partitions as rows arrive.");
        }
        Some(_) if existing.added.is_empty() && existing.add_blocked().is_some() => {
            ui.weak(existing.add_blocked().unwrap_or_default());
        }
        Some(method) => {
            ui.label(RichText::new("New partitions").strong());
            partitions_grid(
                ui,
                "added-partitions",
                method,
                &mut existing.added,
                tablespaces,
            );
            add_buttons(ui, method, &mut existing.added);
        }
        None => {
            ui.weak("Adding partitions to this kind of table isn't supported here.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(name: &str, bound: &str) -> PartitionDef {
        PartitionDef {
            name: name.into(),
            bound: bound.into(),
            tablespace: String::new(),
        }
    }

    #[test]
    fn validates_new_partitioning() {
        let columns = ["ID".to_owned(), "CREATED".to_owned()];
        let mut p = Partitioning {
            method: PartitionMethod::Range,
            partitions: vec![part("p1", "100")],
            ..Partitioning::default()
        };
        assert!(validate_new(&p, "", &columns, false).is_err());
        assert!(validate_new(&p, "nope", &columns, false).is_err());
        assert!(validate_new(&p, "id", &columns, true).is_err(), "temporary");
        assert_eq!(validate_new(&p, "id", &columns, false), Ok(()));
        p.partitions.push(part("P1", "200"));
        assert_eq!(
            validate_new(&p, "id", &columns, false),
            Err("Partition P1 appears twice".into())
        );
        p.partitions[1].name = "p2".into();
        p.partitions[1].bound.clear();
        assert!(validate_new(&p, "id", &columns, false).is_err());
        p.method = PartitionMethod::Hash;
        p.partitions.clear();
        assert_eq!(validate_new(&p, "id", &columns, false), Ok(()));
        p.method = PartitionMethod::Interval;
        p.partitions = vec![part("p1", "DATE '2026-01-01'")];
        assert!(
            validate_new(&p, "created", &columns, false).is_err(),
            "no interval"
        );
        p.interval = "NUMTOYMINTERVAL(1, 'MONTH')".into();
        assert_eq!(validate_new(&p, "created", &columns, false), Ok(()));
        let d = to_dictionary(&p, "created");
        assert_eq!(d.columns, ["CREATED"]);
        assert_eq!(d.partitions[0].name, "P1");
    }

    #[test]
    fn changes_existing_partitions() {
        let mut e = Existing {
            kind: "RANGE".into(),
            method: Some(PartitionMethod::Range),
            columns: vec!["ID".into()],
            partitions: vec![(part("P1", "100"), false), (part("P2", "200"), false)],
            ..Existing::default()
        };
        assert!(!e.is_changed());
        e.actions.insert("P1".into(), Action::Truncate);
        e.actions.insert("P2".into(), Action::Drop);
        e.added.push(part("p3", "300"));
        let s = statements("ORDERS", &e, false).unwrap();
        let sql: Vec<(&str, bool)> = s.iter().map(|s| (s.sql.as_str(), s.enabled)).collect();
        assert_eq!(
            sql,
            [
                ("alter table orders truncate partition p1", false),
                ("alter table orders drop partition p2", false),
                (
                    "alter table orders add partition p3 values less than (300)",
                    true
                ),
            ]
        );
        assert!(statements("ORDERS", &e, true).unwrap()[1].enabled);
        e.method = Some(PartitionMethod::Interval);
        assert!(statements("ORDERS", &e, true).is_err());
        // A MAXVALUE partition takes everything above: none can be added,
        // unless it's dropped.
        e.method = Some(PartitionMethod::Range);
        e.partitions.push((part("P_MAX", "MAXVALUE"), false));
        assert!(e.add_blocked().unwrap().contains("p_max (MAXVALUE)"));
        assert!(statements("ORDERS", &e, true).is_err());
        e.actions.insert("P_MAX".into(), Action::Drop);
        assert!(e.add_blocked().is_none());
        e.partitions.pop();
        e.actions.remove("P_MAX");
        e.method = Some(PartitionMethod::Interval);
        // A copy keeps the partitions defined, not those an interval added.
        e.partitions.push((part("SYS_P41", "300"), true));
        let copy = e.copy().unwrap();
        assert_eq!(copy.partitions.len(), 2);
    }
}
