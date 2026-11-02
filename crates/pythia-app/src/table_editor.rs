//! Table editor: builds a new table, or changes an existing one's columns,
//! keys, indexes, comments and partitions. A new table becomes `CREATE TABLE` and the
//! statements that follow it; an existing one the ALTER script from diffing
//! against the table as loaded, with the same structural diff as schema
//! compare. Apply runs the script on the window's session.

use std::collections::{HashMap, HashSet};

use egui::{RichText, Ui};
use pythia_compare::script::{ScriptHeader, SyncStatement, render};
use pythia_compare::table::{
    self, ColumnDef, ConstraintDef, ConstraintKind, CreateOptions, IndexDef, TableDef, Temporary,
    diff,
};
use pythia_db::oracle::Connection;
use pythia_db::{DbError, DbEvent, DbRequest, ObjectType};
use pythia_ui::icons::Icon;
use serde::{Deserialize, Serialize};

use crate::documents::{
    DocContext, Effects, ObjectRef, OpenRequest, Target, quote_ident, request_id,
};
use crate::object_editor::{ApplyError, first_line, ident, names, picker};
use crate::table_partitions::{self, Existing};

#[derive(Serialize, Deserialize)]
pub struct TableEditor {
    /// The table; a new one has its owner and, until created, no name.
    pub object: ObjectRef,
    /// A new table, not created yet.
    #[serde(default)]
    pub creating: bool,
    /// A new table starts as a copy of this one.
    #[serde(default)]
    pub like: Option<ObjectRef>,
    #[serde(skip)]
    state: State,
}

/// The editor's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TableTab {
    #[default]
    Columns,
    Keys,
    Indexes,
    Partitions,
    Script,
}

#[derive(Default)]
struct State {
    loading: Option<u64>,
    /// The table as loaded, names as in the dictionary; none for a new one.
    original: Option<TableDef>,
    /// The form, names as typed (`EMP`, `"Mixed"`).
    edited: Option<TableDef>,
    /// By column of the form: its name as loaded (none for a new column),
    /// to tell a renamed column from a new one.
    origins: Vec<Option<String>>,
    error: Option<String>,
    tab: TableTab,
    allow_drops: bool,
    /// Tablespace (as typed) and temporary, for a new table.
    options: CreateOptions,
    /// A new table's partitioning (names as typed) and its key columns.
    partitioning: Option<pythia_compare::table::Partitioning>,
    partition_key: String,
    /// An existing table's partitions and the changes to them.
    partitions: Option<Existing>,
    new_constraint: NewConstraint,
    new_index: NewIndex,
    lists: Lists,
    /// Key columns being loaded: request → table.
    keys_loading: HashMap<u64, String>,
    applying: Option<u64>,
    /// Result of the last apply: message and success.
    outcome: Option<(String, bool)>,
}

/// Names for the pickers, loaded with the table.
#[derive(Default)]
struct Lists {
    release: pythia_db::compat::Release,
    tables: Vec<String>,
    tablespaces: Vec<String>,
    /// Primary (else unique) key columns by table, for foreign keys.
    keys: HashMap<String, Vec<String>>,
    /// A table whose keys the form asked for this frame.
    want_keys: Option<String>,
}

struct Loaded {
    table: Option<TableDef>,
    partitions: Option<Existing>,
    release: pythia_db::compat::Release,
    tables: Vec<String>,
    tablespaces: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ConstraintKindChoice {
    Primary,
    #[default]
    Unique,
    Check,
    Foreign,
}

impl ConstraintKindChoice {
    fn label(self) -> &'static str {
        match self {
            ConstraintKindChoice::Primary => "Primary key",
            ConstraintKindChoice::Unique => "Unique",
            ConstraintKindChoice::Check => "Check",
            ConstraintKindChoice::Foreign => "Foreign key",
        }
    }
}

/// The "new constraint" row of the keys tab.
#[derive(Default)]
struct NewConstraint {
    name: String,
    kind: ConstraintKindChoice,
    columns: String,
    condition: String,
    ref_table: String,
    ref_columns: String,
    /// `CASCADE` or `SET NULL`.
    on_delete: Option<&'static str>,
}

/// The "new index" row of the indexes tab.
#[derive(Default)]
struct NewIndex {
    name: String,
    unique: bool,
    columns: String,
}

impl TableEditor {
    pub fn new(object: ObjectRef) -> Self {
        Self {
            object,
            creating: false,
            like: None,
            state: State::default(),
        }
    }

    /// A new table in `owner`, empty or as a copy of `like`.
    pub fn create(owner: String, like: Option<ObjectRef>) -> Self {
        Self {
            object: ObjectRef {
                owner,
                name: String::new(),
                object_type: ObjectType::Table,
            },
            creating: true,
            like,
            state: State::default(),
        }
    }

    /// Shows `tab` first.
    pub fn with_tab(mut self, tab: TableTab) -> Self {
        self.state.tab = tab;
        self
    }

    pub fn title(&self) -> String {
        // The tab adds the unsaved-changes dot.
        if self.creating {
            "New table".to_owned()
        } else {
            // Oracle's tables (Feature::ObjectEditors), named as shown (#272).
            let name = crate::browser::shown_name(pythia_db::Dialect::ORACLE, &self.object.name);
            format!("{name} (edit)")
        }
    }

    pub fn is_busy(&self) -> bool {
        self.state.loading.is_some()
            || self.state.applying.is_some()
            || !self.state.keys_loading.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        if self
            .state
            .partitions
            .as_ref()
            .is_some_and(Existing::is_changed)
        {
            return true;
        }
        match (&self.state.original, &self.state.edited) {
            (Some(a), Some(b)) => *a != to_dictionary(b),
            (None, Some(b)) => !b.name.trim().is_empty(),
            _ => false,
        }
    }

    #[cfg(test)]
    pub fn outcome(&self) -> Option<(&str, bool)> {
        self.state.outcome.as_ref().map(|(m, ok)| (m.as_str(), *ok))
    }

    /// Changes the form in tests.
    #[cfg(test)]
    pub fn edit_for_test(&mut self, f: impl FnOnce(&mut TableDef)) {
        if let Some(def) = self.state.edited.as_mut() {
            f(def);
        }
    }

    /// The partition key typed on the Partitions tab, in tests.
    #[cfg(test)]
    pub fn set_partition_key_for_test(&mut self, key: &str) {
        self.state.partition_key = key.into();
    }

    /// Changes a new table's partitioning in tests.
    #[cfg(test)]
    pub fn edit_partitioning_for_test(
        &mut self,
        f: impl FnOnce(&mut pythia_compare::table::Partitioning),
    ) {
        if let Some(p) = self.state.partitioning.as_mut() {
            f(p);
        }
    }

    #[cfg(test)]
    pub fn partition_names_for_test(&self) -> Vec<String> {
        self.state
            .partitions
            .iter()
            .flat_map(|e| e.partitions.iter().map(|(p, _)| p.name.clone()))
            .collect()
    }

    /// The statements Apply would run, in order.
    #[cfg(test)]
    pub fn statements_for_test(&self) -> Result<Vec<String>, String> {
        Ok(ordered(self.statements()?.0))
    }

    #[cfg(test)]
    pub fn apply_for_test(&mut self) -> Effects {
        self.apply()
    }

    pub fn load(&mut self) -> Effects {
        let mut effects = Effects::default();
        let id = request_id();
        self.state.loading = Some(id);
        self.state.error = None;
        let owner = self.object.owner.clone();
        let source = match (self.creating, &self.like) {
            (false, _) => Some((self.object.owner.clone(), self.object.name.clone())),
            (true, Some(like)) => Some((like.owner.clone(), like.name.clone())),
            (true, None) => None,
        };
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            let loaded = load(conn, &owner, source)?;
            Ok(Box::new(loaded) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Meta, DbRequest::Task { id, task });
        effects
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        let DbEvent::TaskDone { id, result } = event else {
            return Effects::default();
        };
        if self.state.loading == Some(id) {
            self.state.loading = None;
            match result.map(|any| any.downcast::<Loaded>()) {
                Ok(Ok(loaded)) => self.loaded(*loaded),
                Ok(Err(_)) => self.state.error = Some("unexpected result".into()),
                Err(e) => self.state.error = Some(e.to_string()),
            }
        } else if let Some(table) = self.state.keys_loading.remove(&id) {
            let columns = result
                .ok()
                .and_then(|any| any.downcast::<Vec<String>>().ok())
                .map(|c| *c)
                .unwrap_or_default();
            let new = &mut self.state.new_constraint;
            if ident(&new.ref_table) == table && new.ref_columns.trim().is_empty() {
                new.ref_columns = typed_list(&columns);
            }
            self.state.lists.keys.insert(table, columns);
        } else if self.state.applying == Some(id) {
            self.state.applying = None;
            match result.map(|any| any.downcast::<Result<(), ApplyError>>()) {
                Ok(Ok(outcome)) => match *outcome {
                    Ok(()) => {
                        self.state.outcome = Some(("Applied".into(), true));
                        if self.creating
                            && let Some(def) = &self.state.edited
                        {
                            self.object.name = ident(&def.name);
                            self.creating = false;
                            self.like = None;
                        }
                        let mut effects = self.load();
                        effects.changed = Some(self.object.clone());
                        return effects;
                    }
                    Err(e) => {
                        self.state.outcome = Some((
                            format!("{}\n\nin: {}", e.error.text(), first_line(&e.statement)),
                            false,
                        ));
                    }
                },
                Ok(Err(_)) => self.state.outcome = Some(("unexpected result".into(), false)),
                Err(e) => self.state.outcome = Some((e.to_string(), false)),
            }
        }
        Effects::default()
    }

    fn loaded(&mut self, loaded: Loaded) {
        let lists = &mut self.state.lists;
        lists.release = loaded.release;
        lists.tables = loaded.tables;
        lists.tablespaces = loaded.tablespaces;
        match (self.creating, loaded.table) {
            (false, Some(table)) => {
                self.state.edited = Some(to_typed(&table));
                self.state.origins = table.columns.iter().map(|c| Some(c.name.clone())).collect();
                self.state.original = Some(table);
                self.state.partitions = loaded.partitions;
            }
            (true, like) => {
                self.state.original = None;
                // A copy is partitioned the same way.
                self.state.partitioning = loaded.partitions.as_ref().and_then(Existing::copy);
                if let Some(p) = &self.state.partitioning {
                    self.state.partition_key = p.columns.join(", ");
                }
                self.state.edited = Some(match like {
                    Some(table) => copy_of(&table),
                    None => new_table(),
                });
            }
            (false, None) => self.state.error = Some("unexpected result".into()),
        }
    }

    /// The statements turning the loaded table into the edited one (or
    /// creating it), and notes about differences they leave out.
    fn statements(&self) -> Result<(Vec<SyncStatement>, Vec<String>), String> {
        let Some(edited) = &self.state.edited else {
            return Ok(Default::default());
        };
        let release = self.state.lists.release;
        validate(edited, self.creating, release)?;
        let def = named(to_dictionary(edited), max_name_length(release));
        if self.creating {
            let key = &self.state.partition_key;
            if let Some(p) = &self.state.partitioning {
                let columns: Vec<String> = def.columns.iter().map(|c| c.name.clone()).collect();
                let temporary = self.state.options.temporary != Temporary::No;
                table_partitions::validate_new(p, key, &columns, temporary)?;
            }
            let options = CreateOptions {
                tablespace: ident(&self.state.options.tablespace),
                temporary: self.state.options.temporary,
                partitioning: self
                    .state
                    .partitioning
                    .as_ref()
                    .map(|p| table_partitions::to_dictionary(p, key)),
            };
            return Ok((table::create(&def, &options), Vec::new()));
        }
        let Some(original) = &self.state.original else {
            return Ok(Default::default());
        };
        // Renamed columns: renamed first, then compared under the new name.
        let renames = renames(&def, &self.state.origins)?;
        let mut base = original.clone();
        apply_renames(&mut base, &renames);
        let mut def = def;
        for (from, to) in &renames {
            rename_in_indexes(&mut def, from, to);
        }
        let t = table::ident(&original.name);
        let mut d = diff(&def, &base, self.state.allow_drops);
        let mut statements: Vec<SyncStatement> = renames
            .iter()
            .map(|(from, to)| {
                SyncStatement::new(
                    pythia_compare::script::Phase::Tables,
                    original.name.clone(),
                    format!(
                        "alter table {t} rename column {} to {}",
                        table::ident(from),
                        table::ident(to)
                    ),
                )
            })
            .collect();
        statements.append(&mut d.statements);
        d.statements = statements;
        if let Some(existing) = &self.state.partitions {
            d.statements.extend(table_partitions::statements(
                &original.name,
                existing,
                self.state.allow_drops,
            )?);
        }
        Ok((d.statements, d.notes))
    }

    fn script(&self, statements: &[SyncStatement]) -> String {
        let name = match &self.state.edited {
            Some(def) if self.creating => ident(&def.name),
            _ => self.object.name.clone(),
        };
        let verb = if self.creating {
            "Creates"
        } else {
            "Changes to"
        };
        let header = ScriptHeader {
            title: format!("{verb} {}.{name} (generated by Pythia)", self.object.owner),
            schema: Some(self.object.owner.clone()),
            recompile: false,
        };
        render(&header, statements)
    }

    /// Runs the script on the window's session, in the table's schema.
    fn apply(&mut self) -> Effects {
        let mut effects = Effects::default();
        let Ok((statements, _)) = self.statements() else {
            return effects;
        };
        let sql = ordered(statements);
        if sql.is_empty() {
            return effects;
        }
        let owner = self.object.owner.clone();
        let id = request_id();
        self.state.applying = Some(id);
        self.state.outcome = None;
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            let outcome = run(conn, &owner, &sql);
            Ok(Box::new(outcome) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    /// Loads the key columns a foreign key's table asked for.
    fn request_keys(&mut self, effects: &mut Effects) {
        let Some(table) = self.state.lists.want_keys.take() else {
            return;
        };
        if self.state.lists.keys.contains_key(&table)
            || self.state.keys_loading.values().any(|t| *t == table)
        {
            return;
        }
        let id = request_id();
        self.state.keys_loading.insert(id, table.clone());
        let owner = self.object.owner.clone();
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            let columns = key_columns(conn, &owner, &table)?;
            Ok(Box::new(columns) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Meta, DbRequest::Task { id, task });
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        if let Some(e) = &self.state.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
            if ui.button("Retry").clicked() {
                effects.extend(self.load());
            }
            return effects;
        }
        if self.state.edited.is_none() {
            ui.spinner();
            return effects;
        }
        let statements = self.statements();
        let (script, notes) = match &statements {
            Ok((s, notes)) if !s.is_empty() => (self.script(s), notes.clone()),
            Ok((_, notes)) => (String::new(), notes.clone()),
            Err(_) => (String::new(), Vec::new()),
        };
        ui.horizontal(|ui| {
            ui.add(Icon::Table.sized(ui, pythia_ui::icons::IconSize::Small));
            let heading = if self.creating {
                format!("New table in {}", self.object.owner)
            } else {
                format!("{}.{}", self.object.owner, self.object.name)
            };
            ui.label(RichText::new(heading).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = !script.is_empty();
                let apply = egui::Button::image_and_text(Icon::Execute.image(ui), "Apply");
                if ui
                    .add_enabled(ready && cx.connected && !self.is_busy(), apply)
                    .on_hover_text("Run the script in this window's session")
                    .clicked()
                {
                    effects.extend(self.apply());
                }
                if self.state.applying.is_some() {
                    ui.spinner();
                }
                if ui
                    .add_enabled(ready, egui::Button::new("Open in SQL window"))
                    .clicked()
                    && let Some(connection) = cx.connection.clone()
                {
                    effects.open.push(OpenRequest::Sql {
                        connection,
                        text: script.clone(),
                        run: false,
                    });
                }
                if !self.creating {
                    if ui
                        .add_enabled(self.is_dirty(), egui::Button::new("Revert"))
                        .clicked()
                    {
                        self.state.edited = self.state.original.as_ref().map(to_typed);
                        self.state.origins = self
                            .state
                            .original
                            .iter()
                            .flat_map(|o| o.columns.iter().map(|c| Some(c.name.clone())))
                            .collect();
                        if let Some(p) = &mut self.state.partitions {
                            p.added.clear();
                            p.actions.clear();
                        }
                    }
                    if ui.button("Reload").clicked() {
                        effects.extend(self.load());
                    }
                }
            });
        });
        if self.creating {
            self.options_ui(ui);
        }
        if let Some((message, ok)) = &self.state.outcome {
            let color = if *ok {
                egui::Color32::from_rgb(0x2f, 0xa0, 0x4c)
            } else {
                ui.visuals().error_fg_color
            };
            ui.label(RichText::new(message).color(color));
        }
        if let Err(problem) = &statements {
            ui.label(RichText::new(problem).color(ui.visuals().warn_fg_color));
        }
        ui.horizontal(|ui| {
            let tab = &mut self.state.tab;
            ui.selectable_value(tab, TableTab::Columns, "Columns");
            ui.selectable_value(tab, TableTab::Keys, "Keys & checks");
            ui.selectable_value(tab, TableTab::Indexes, "Indexes");
            ui.selectable_value(tab, TableTab::Partitions, "Partitions");
            let label = if script.is_empty() {
                "Script"
            } else {
                "Script •"
            };
            ui.selectable_value(tab, TableTab::Script, label);
        });
        ui.separator();
        let creating = self.creating;
        let lists = &mut self.state.lists;
        let table = self.state.edited.as_mut().expect("loaded");
        match self.state.tab {
            TableTab::Columns => {
                columns_ui(ui, table, &mut self.state.origins, creating, lists.release)
            }
            TableTab::Keys => keys_ui(ui, table, &mut self.state.new_constraint, lists),
            TableTab::Indexes => indexes_ui(ui, table, &mut self.state.new_index),
            TableTab::Partitions => {
                if creating {
                    let columns: Vec<String> =
                        table.columns.iter().map(|c| c.name.clone()).collect();
                    table_partitions::new_table_ui(
                        ui,
                        &mut self.state.partitioning,
                        &mut self.state.partition_key,
                        &columns,
                        &lists.tablespaces,
                    );
                } else if let Some(existing) = &mut self.state.partitions {
                    table_partitions::existing_ui(ui, existing, &lists.tablespaces);
                } else {
                    ui.weak("Not partitioned.");
                    ui.weak("Turning a table into a partitioned one (ALTER TABLE … MODIFY PARTITION BY, 12.2+) isn't offered here.");
                }
            }
            TableTab::Script => {
                if !creating {
                    ui.checkbox(&mut self.state.allow_drops, "Allow dropping")
                        .on_hover_text(
                            "Dropping columns, and dropping or truncating partitions, loses data; off by default",
                        );
                }
                for note in &notes {
                    ui.colored_label(ui.visuals().warn_fg_color, note);
                }
                match &statements {
                    Err(problem) => {
                        ui.colored_label(ui.visuals().warn_fg_color, problem);
                    }
                    Ok(_) if script.is_empty() => {
                        ui.weak("No changes");
                    }
                    Ok(_) => {
                        let theme = pythia_ui::SyntaxTheme::for_visuals(ui.visuals());
                        let font = egui::FontId::monospace(cx.settings.editor.font_size.min(14.0));
                        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                            let job = pythia_ui::highlight(&script, font, &theme);
                            ui.add(egui::Label::new(job).extend());
                        });
                    }
                }
            }
        }
        self.request_keys(&mut effects);
        effects
    }

    /// Name, tablespace and kind of a new table.
    fn options_ui(&mut self, ui: &mut Ui) {
        let Some(table) = self.state.edited.as_mut() else {
            return;
        };
        let options = &mut self.state.options;
        let tablespaces = &self.state.lists.tablespaces;
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Name");
            let response = ui.add(
                egui::TextEdit::singleline(&mut table.name)
                    .hint_text("table name")
                    .font(egui::TextStyle::Monospace)
                    .desired_width(220.0),
            );
            if table.name.is_empty()
                && !response.has_focus()
                && ui.memory(|m| m.focused().is_none())
            {
                response.request_focus();
            }
            ui.add_space(12.0);
            let temporary = options.temporary != Temporary::No;
            ui.add_enabled_ui(!temporary, |ui| {
                ui.label("Tablespace");
                ui.add(
                    egui::TextEdit::singleline(&mut options.tablespace)
                        .hint_text("default")
                        .desired_width(140.0),
                );
                if let Some(ts) = picker(ui, "tablespace", tablespaces) {
                    options.tablespace = quote_ident(&ts);
                }
            });
            ui.add_space(12.0);
            egui::ComboBox::from_id_salt("table-kind")
                .selected_text(match options.temporary {
                    Temporary::No => "Table",
                    Temporary::DeleteRows => "Temporary, rows until commit",
                    Temporary::PreserveRows => "Temporary, rows until the session ends",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut options.temporary, Temporary::No, "Table");
                    ui.selectable_value(
                        &mut options.temporary,
                        Temporary::DeleteRows,
                        "Temporary, rows until commit",
                    )
                    .on_hover_text("Global temporary table: ON COMMIT DELETE ROWS");
                    ui.selectable_value(
                        &mut options.temporary,
                        Temporary::PreserveRows,
                        "Temporary, rows until the session ends",
                    )
                    .on_hover_text("Global temporary table: ON COMMIT PRESERVE ROWS");
                });
        });
        ui.add_space(2.0);
    }
}

fn load(
    conn: &Connection,
    owner: &str,
    source: Option<(String, String)>,
) -> Result<Loaded, DbError> {
    let release = pythia_db::compat::release(conn).map_err(DbError::from)?;
    let tables = names(
        conn,
        "select table_name from all_tables
          where owner = :1 and nested = 'NO' and secondary = 'N'
            and table_name not like 'BIN$%'
          order by table_name",
        &[&owner],
    )?;
    let tablespaces = names(
        conn,
        "select tablespace_name from user_tablespaces
          where contents = 'PERMANENT' order by tablespace_name",
        &[],
    )?;
    let (table, partitions) = match source {
        Some((owner, name)) => {
            let mut tables = table::load(conn, &owner, Some(&name)).map_err(DbError::from)?;
            let table = tables
                .remove(&name)
                .ok_or_else(|| DbError::new(format!("Table {owner}.{name} not found")))?;
            (Some(table), table_partitions::load(conn, &owner, &name)?)
        }
        None => (None, None),
    };
    Ok(Loaded {
        table,
        partitions,
        release,
        tables,
        tablespaces,
    })
}

/// The primary key's columns of a table, else those of its first unique key.
fn key_columns(conn: &Connection, owner: &str, table: &str) -> Result<Vec<String>, DbError> {
    let mut keys: Vec<(String, String, String)> = Vec::new();
    for row in conn.query_as::<(String, String, String)>(
        "select k.constraint_type, k.constraint_name, c.column_name
           from all_constraints k
           join all_cons_columns c on c.owner = k.owner and c.constraint_name = k.constraint_name
          where k.owner = :1 and k.table_name = :2 and k.constraint_type in ('P', 'U')
          order by k.constraint_type, k.constraint_name, c.position",
        &[&owner, &table],
    )? {
        keys.push(row?);
    }
    let Some((_, first, _)) = keys.first().cloned() else {
        return Ok(Vec::new());
    };
    Ok(keys
        .into_iter()
        .filter(|(_, name, _)| *name == first)
        .map(|(_, _, column)| column)
        .collect())
}

/// Runs `statements` in order in `owner`'s schema, stopping at the first
/// error.
fn run(conn: &Connection, owner: &str, statements: &[String]) -> Result<(), ApplyError> {
    let schema = format!("alter session set current_schema = {}", quote_ident(owner));
    for s in std::iter::once(&schema).chain(statements) {
        conn.execute(s, &[]).map_err(|e| ApplyError {
            statement: s.clone(),
            error: e.into(),
            dropped: false,
        })?;
    }
    Ok(())
}

/// The statements to run, in script order (destructive ones left out
/// unless enabled).
fn ordered(mut statements: Vec<SyncStatement>) -> Vec<String> {
    statements.retain(|s| s.enabled);
    statements.sort_by_key(|s| s.phase);
    statements.into_iter().map(|s| s.sql).collect()
}

/// A new table's starting point: a numeric primary key column.
fn new_table() -> TableDef {
    TableDef {
        name: String::new(),
        columns: vec![ColumnDef {
            name: "ID".into(),
            data_type: "NUMBER(10)".into(),
            nullable: false,
            default: None,
            comment: None,
            identity: false,
            generation: Default::default(),
        }],
        comment: None,
        constraints: vec![ConstraintDef {
            name: String::new(),
            kind: ConstraintKind::Primary,
            columns: vec!["ID".into()],
            references: None,
            on_delete: None,
            condition: None,
            generated: false,
        }],
        indexes: Vec::new(),
    }
}

/// A table to start a new one from: its structure, without names that
/// must be unique in the schema (they are generated from the new name).
fn copy_of(table: &TableDef) -> TableDef {
    let mut def = to_typed(table);
    def.name.clear();
    for c in &mut def.constraints {
        if !c.generated {
            c.name.clear();
        }
    }
    for i in &mut def.indexes {
        i.name.clear();
        i.generated = false;
    }
    def
}

/// The dictionary names of a table (as loaded) in the form they are typed:
/// `EMP`, `"Mixed"`.
fn to_typed(def: &TableDef) -> TableDef {
    convert(def, |name| {
        if name.is_empty() {
            String::new()
        } else {
            quote_ident(name)
        }
    })
}

/// The typed names of the form as the dictionary has them.
fn to_dictionary(def: &TableDef) -> TableDef {
    convert(def, ident)
}

/// Converts the names of a table, its columns, constraints and indexes.
/// Generated names, check conditions, defaults and index expressions stay.
fn convert(def: &TableDef, f: impl Fn(&str) -> String) -> TableDef {
    let list = |names: &[String]| names.iter().map(|n| f(n)).collect();
    TableDef {
        name: f(&def.name),
        columns: def
            .columns
            .iter()
            .map(|c| ColumnDef {
                name: f(&c.name),
                ..c.clone()
            })
            .collect(),
        comment: def.comment.clone(),
        constraints: def
            .constraints
            .iter()
            .map(|c| ConstraintDef {
                name: if c.generated {
                    c.name.clone()
                } else {
                    f(&c.name)
                },
                columns: list(&c.columns),
                references: c
                    .references
                    .as_ref()
                    .map(|(table, columns)| (f(table), list(columns))),
                ..c.clone()
            })
            .collect(),
        indexes: def
            .indexes
            .iter()
            .map(|i| IndexDef {
                name: if i.generated {
                    i.name.clone()
                } else {
                    f(&i.name)
                },
                ..i.clone()
            })
            .collect(),
    }
}

/// Longest name the database takes: 30 bytes before 12.2, 128 after.
fn max_name_length(release: pythia_db::compat::Release) -> usize {
    if release >= (12, 2) { 128 } else { 30 }
}

/// Gives unnamed constraints and indexes a name after the table:
/// `ORDERS_PK`, `ORDERS_UK`, `ORDERS_FK2`, `ORDERS_IX`.
fn named(mut def: TableDef, max: usize) -> TableDef {
    let mut taken: HashSet<String> = def
        .constraints
        .iter()
        .map(|c| c.name.clone())
        .chain(def.indexes.iter().map(|i| i.name.clone()))
        .filter(|n| !n.is_empty())
        .collect();
    let base = def.name.clone();
    let mut next = |suffix: &str| -> String {
        for n in 1.. {
            let tail = if n == 1 {
                format!("_{suffix}")
            } else {
                format!("_{suffix}{n}")
            };
            let mut head = base.clone();
            if head.len() + tail.len() > max {
                while head.len() + tail.len() > max {
                    head.pop();
                }
                // `ORDERS_` + `_IX2` reads better as `ORDERS_IX2`.
                while head.ends_with('_') {
                    head.pop();
                }
            }
            let name = format!("{head}{tail}");
            if taken.insert(name.clone()) {
                return name;
            }
        }
        unreachable!()
    };
    for c in &mut def.constraints {
        if c.name.is_empty() && !c.generated {
            c.name = next(match c.kind {
                ConstraintKind::Primary => "PK",
                ConstraintKind::Unique => "UK",
                ConstraintKind::Foreign => "FK",
                ConstraintKind::Check => "CK",
            });
        }
    }
    for i in &mut def.indexes {
        if i.name.is_empty() {
            i.name = next("IX");
        }
    }
    def
}

/// What keeps the form from becoming a script, as a hint for the user.
fn validate(
    def: &TableDef,
    creating: bool,
    release: pythia_db::compat::Release,
) -> Result<(), String> {
    if creating {
        if def.name.trim().is_empty() {
            return Err("Enter the table's name".into());
        }
        if ident(&def.name).len() > max_name_length(release) {
            return Err(format!(
                "The name is longer than {} bytes",
                max_name_length(release)
            ));
        }
    }
    if def.columns.is_empty() {
        return Err("Add a column".into());
    }
    let mut seen = HashSet::new();
    for (i, c) in def.columns.iter().enumerate() {
        if c.name.trim().is_empty() {
            return Err(format!("Column {} has no name", i + 1));
        }
        let name = ident(&c.name);
        if !seen.insert(name.clone()) {
            return Err(format!("Column {name} appears twice"));
        }
        if c.data_type.trim().is_empty() {
            return Err(format!("Column {name} has no type"));
        }
        if c.identity {
            if release < (12, 1) {
                return Err("Identity columns need Oracle 12c or later".into());
            }
            if !is_numeric(&c.data_type) {
                return Err(format!("Identity column {name} must be a number"));
            }
        }
    }
    if def
        .constraints
        .iter()
        .filter(|c| c.kind == ConstraintKind::Primary)
        .count()
        > 1
    {
        return Err("A table has only one primary key".into());
    }
    for c in &def.constraints {
        let what = match c.kind {
            ConstraintKind::Primary => "The primary key",
            ConstraintKind::Unique => "A unique key",
            ConstraintKind::Foreign => "A foreign key",
            ConstraintKind::Check => "A check",
        };
        if c.kind == ConstraintKind::Check {
            if c.condition.as_deref().is_none_or(|t| t.trim().is_empty()) {
                return Err(format!("{what} has no condition"));
            }
            continue;
        }
        if c.columns.is_empty() {
            return Err(format!("{what} has no columns"));
        }
        if let Some(missing) = c.columns.iter().find(|n| !seen.contains(&ident(n))) {
            return Err(format!("{what} uses {missing}, which isn't a column"));
        }
        if c.kind == ConstraintKind::Foreign {
            match &c.references {
                Some((table, _)) if table.trim().is_empty() => {
                    return Err(format!("{what} has no referenced table"));
                }
                Some((_, columns)) if columns.len() != c.columns.len() => {
                    return Err(format!(
                        "{what} needs as many referenced columns as columns"
                    ));
                }
                None => return Err(format!("{what} has no referenced table")),
                Some(_) => {}
            }
        }
    }
    for i in &def.indexes {
        if i.columns.is_empty() {
            return Err("An index has no columns".into());
        }
    }
    Ok(())
}

fn is_numeric(data_type: &str) -> bool {
    let t = data_type.trim().to_ascii_uppercase();
    ["NUMBER", "INTEGER", "INT", "SMALLINT", "DECIMAL", "NUMERIC"]
        .iter()
        .any(|n| t == *n || t.starts_with(&format!("{n}(")) || t.starts_with(&format!("{n} (")))
}

/// Column types offered by the picker, for this release.
fn data_types(release: pythia_db::compat::Release) -> Vec<String> {
    let mut types: Vec<&str> = vec![
        "VARCHAR2(100 CHAR)",
        "VARCHAR2(4000 CHAR)",
        "NUMBER",
        "NUMBER(10)",
        "NUMBER(12,2)",
        "DATE",
        "TIMESTAMP",
        "TIMESTAMP WITH TIME ZONE",
        "TIMESTAMP WITH LOCAL TIME ZONE",
        "CHAR(1 CHAR)",
        "NVARCHAR2(100)",
        "CLOB",
        "NCLOB",
        "BLOB",
        "RAW(16)",
        "BINARY_DOUBLE",
        "INTERVAL DAY TO SECOND",
    ];
    if release >= (21, 0) {
        types.push("JSON");
    }
    if release >= (23, 0) {
        types.push("BOOLEAN");
    }
    types.into_iter().map(str::to_owned).collect()
}

/// Typed names as a comma separated list.
fn typed_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| quote_ident(n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A typed, comma separated list of names.
fn split_columns(text: &str) -> Vec<String> {
    text.split(',')
        .map(|c| c.trim().to_owned())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Appends a picked name to a comma separated list.
fn append(list: &mut String, name: &str) {
    if !list.trim().is_empty() {
        list.push_str(", ");
    }
    list.push_str(&quote_ident(name));
}

fn primary_key(table: &TableDef) -> Option<&ConstraintDef> {
    table
        .constraints
        .iter()
        .find(|c| c.kind == ConstraintKind::Primary)
}

/// Puts a column into the primary key or takes it out; the key is created
/// or dropped with its first or last column.
fn set_primary(table: &mut TableDef, column: &str, on: bool) {
    let same = |n: &String| ident(n) == ident(column);
    match table
        .constraints
        .iter()
        .position(|c| c.kind == ConstraintKind::Primary)
    {
        Some(i) if on => {
            if !table.constraints[i].columns.iter().any(same) {
                table.constraints[i].columns.push(column.to_owned());
            }
        }
        Some(i) => {
            table.constraints[i].columns.retain(|n| !same(n));
            if table.constraints[i].columns.is_empty() {
                table.constraints.remove(i);
            }
        }
        None if on => table.constraints.push(ConstraintDef {
            name: String::new(),
            kind: ConstraintKind::Primary,
            columns: vec![column.to_owned()],
            references: None,
            on_delete: None,
            condition: None,
            generated: false,
        }),
        None => {}
    }
}

/// Columns of the form whose name changed since loading: (from, to), as
/// in the dictionary.
fn renames(def: &TableDef, origins: &[Option<String>]) -> Result<Vec<(String, String)>, String> {
    let renames: Vec<(String, String)> = def
        .columns
        .iter()
        .zip(origins)
        .filter_map(|(c, origin)| {
            let origin = origin.as_ref()?;
            (c.name != *origin).then(|| (origin.clone(), c.name.clone()))
        })
        .collect();
    // One at a time: a name another column still has would clash.
    for (from, to) in &renames {
        if origins.iter().flatten().any(|o| o == to && o != from) {
            return Err(format!(
                "{to} is still another column's name: rename that one first, or go through another name"
            ));
        }
    }
    Ok(renames)
}

/// The table as loaded with columns renamed, keys and indexes along.
fn apply_renames(table: &mut TableDef, renames: &[(String, String)]) {
    for (from, to) in renames {
        let rename = |name: &mut String| {
            if name == from {
                *name = to.clone();
            }
        };
        for c in &mut table.columns {
            rename(&mut c.name);
        }
        for c in &mut table.constraints {
            c.columns.iter_mut().for_each(rename);
            if let Some((t, cols)) = &mut c.references
                && *t == table.name
            {
                cols.iter_mut().for_each(rename);
            }
        }
        rename_in_indexes(table, from, to);
    }
}

/// A column was renamed: its keys follow.
fn rename_column(table: &mut TableDef, from: &str, to: &str) {
    let (from_name, to_name) = (ident(from), ident(to));
    for c in &mut table.constraints {
        for n in &mut c.columns {
            if ident(n) == from_name {
                *n = to.to_owned();
            }
        }
    }
    rename_in_indexes(table, &from_name, &to_name);
}

/// Index columns are expressions: a plain name, or `"NAME"` (maybe with
/// ` desc`) as loaded.
fn rename_in_indexes(table: &mut TableDef, from: &str, to: &str) {
    let quoted = format!("\"{from}\"");
    for i in &mut table.indexes {
        for e in &mut i.columns {
            if let Some(rest) = e.strip_prefix(&quoted) {
                *e = format!("\"{to}\"{rest}");
            } else if ident(e) == from {
                *e = to.to_owned();
            }
        }
    }
}

fn columns_ui(
    ui: &mut Ui,
    table: &mut TableDef,
    origins: &mut Vec<Option<String>>,
    creating: bool,
    release: pythia_db::compat::Release,
) {
    // Columns added in other ways (tests) are new ones.
    origins.resize(table.columns.len(), None);
    ui.horizontal(|ui| {
        ui.label("Table comment");
        let mut comment = table.comment.clone().unwrap_or_default();
        if ui
            .add_sized([480.0, 22.0], egui::TextEdit::singleline(&mut comment))
            .changed()
        {
            table.comment = Some(comment).filter(|c| !c.is_empty());
        }
    });
    ui.add_space(6.0);
    let types = data_types(release);
    let identities = release >= (12, 1);
    let pk: Vec<String> = primary_key(table)
        .map(|k| k.columns.iter().map(|n| ident(n)).collect())
        .unwrap_or_default();
    let mut remove = None;
    let mut swap = None;
    let mut keys = Vec::new();
    let mut renamed = None;
    let n = table.columns.len();
    egui::ScrollArea::both()
        .auto_shrink([false, true])
        .max_height(ui.available_height() - 40.0)
        .show(ui, |ui| {
            egui::Grid::new("table-columns")
                .striped(true)
                .num_columns(9)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    let mut titles = vec!["", "Name", "Type", "Key"];
                    if identities {
                        titles.push("Identity");
                    }
                    titles.extend(["Not null", "Default", "Comment", ""]);
                    for title in titles {
                        ui.strong(title);
                    }
                    ui.end_row();
                    for (i, col) in table.columns.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            if !creating {
                                return;
                            }
                            if ui
                                .add_enabled(i > 0, egui::Button::new("↑").small())
                                .on_hover_text("Move up")
                                .clicked()
                            {
                                swap = Some((i - 1, i));
                            }
                            if ui
                                .add_enabled(i + 1 < n, egui::Button::new("↓").small())
                                .on_hover_text("Move down")
                                .clicked()
                            {
                                swap = Some((i, i + 1));
                            }
                        });
                        let before = col.name.clone();
                        if ui
                            .add_sized(
                                [180.0, 22.0],
                                egui::TextEdit::singleline(&mut col.name)
                                    .font(egui::TextStyle::Monospace),
                            )
                            .changed()
                        {
                            renamed = Some((before, col.name.clone()));
                        }
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [190.0, 22.0],
                                egui::TextEdit::singleline(&mut col.data_type)
                                    .font(egui::TextStyle::Monospace),
                            );
                            if let Some(t) = picker(ui, ("type", i), &types) {
                                col.data_type = t;
                            }
                        });
                        let mut key = pk.contains(&ident(&col.name));
                        if ui
                            .checkbox(&mut key, "")
                            .on_hover_text("Part of the primary key")
                            .changed()
                        {
                            keys.push((col.name.clone(), key));
                            if key {
                                col.nullable = false;
                            }
                        }
                        let new_column = creating || origins.get(i).is_none_or(Option::is_none);
                        if identities {
                            let response = ui
                                .add_enabled(new_column, egui::Checkbox::without_text(&mut col.identity))
                                .on_hover_text("Numbered by the database: GENERATED BY DEFAULT ON NULL AS IDENTITY")
                                .on_disabled_hover_text("Only a new column can become an identity column");
                            if response.changed() && col.identity {
                                col.nullable = false;
                                col.default = None;
                                if !is_numeric(&col.data_type) {
                                    col.data_type = "NUMBER(10)".into();
                                }
                            }
                        }
                        let mut not_null = !col.nullable;
                        if ui
                            .add_enabled(!col.identity, egui::Checkbox::without_text(&mut not_null))
                            .changed()
                        {
                            col.nullable = !not_null;
                        }
                        let mut default = col.default.clone().unwrap_or_default();
                        let identity = col.identity;
                        let changed = ui
                            .add_enabled_ui(!identity, |ui| {
                                ui.add_sized(
                                    [140.0, 22.0],
                                    egui::TextEdit::singleline(&mut default)
                                        .font(egui::TextStyle::Monospace),
                                )
                            })
                            .inner
                            .changed();
                        if changed {
                            col.default = Some(default).filter(|d| !d.trim().is_empty());
                        }
                        let mut comment = col.comment.clone().unwrap_or_default();
                        if ui
                            .add_sized([220.0, 22.0], egui::TextEdit::singleline(&mut comment))
                            .changed()
                        {
                            col.comment = Some(comment).filter(|c| !c.is_empty());
                        }
                        if crate::icons_ext::small_icon_button(ui, Icon::Trash, "Remove column")
                            .clicked()
                        {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                });
        });
    if let Some((from, to)) = renamed {
        rename_column(table, &from, &to);
    }
    for (column, on) in keys {
        set_primary(table, &column, on);
    }
    if let Some((a, b)) = swap {
        table.columns.swap(a, b);
        origins.swap(a, b);
    }
    if let Some(i) = remove {
        let column = table.columns.remove(i);
        origins.remove(i);
        set_primary(table, &column.name, false);
    }
    if pythia_ui::icons::icon_text_button(ui, Icon::Plus, "Add column").clicked() {
        origins.push(None);
        table.columns.push(ColumnDef {
            name: format!("NEW_COLUMN_{}", table.columns.len() + 1),
            data_type: "VARCHAR2(100 CHAR)".into(),
            nullable: true,
            default: None,
            comment: None,
            identity: false,
            generation: Default::default(),
        });
    }
}

fn keys_ui(ui: &mut Ui, table: &mut TableDef, new: &mut NewConstraint, lists: &mut Lists) {
    let mut remove = None;
    egui::Grid::new("table-keys")
        .striped(true)
        .num_columns(4)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            for title in ["Name", "Type", "Definition", ""] {
                ui.strong(title);
            }
            ui.end_row();
            for (i, c) in table.constraints.iter().enumerate() {
                if c.name.is_empty() {
                    ui.weak("automatic");
                } else {
                    ui.label(RichText::new(&c.name).monospace());
                }
                ui.label(match c.kind {
                    ConstraintKind::Primary => "Primary key",
                    ConstraintKind::Unique => "Unique",
                    ConstraintKind::Foreign => "Foreign key",
                    ConstraintKind::Check => "Check",
                });
                let def = match (&c.kind, &c.references, &c.condition) {
                    (ConstraintKind::Foreign, Some((t, cols)), _) => {
                        let on_delete = match c.on_delete.as_deref() {
                            Some(rule) => format!(" on delete {}", rule.to_lowercase()),
                            None => String::new(),
                        };
                        format!(
                            "({}) → {t} ({}){on_delete}",
                            c.columns.join(", "),
                            cols.join(", ")
                        )
                    }
                    (ConstraintKind::Check, _, Some(cond)) => cond.clone(),
                    _ => format!("({})", c.columns.join(", ")),
                };
                ui.label(RichText::new(def).monospace());
                if crate::icons_ext::small_icon_button(ui, Icon::Trash, "Drop constraint").clicked()
                {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some(i) = remove {
        table.constraints.remove(i);
    }
    ui.add_space(8.0);
    let columns: Vec<String> = table.columns.iter().map(|c| ident(&c.name)).collect();
    ui.horizontal_wrapped(|ui| {
        ui.label("New");
        ui.add(
            egui::TextEdit::singleline(&mut new.name)
                .hint_text("name (automatic)")
                .desired_width(160.0),
        );
        egui::ComboBox::from_id_salt("new-constraint-kind")
            .selected_text(new.kind.label())
            .show_ui(ui, |ui| {
                for kind in [
                    ConstraintKindChoice::Primary,
                    ConstraintKindChoice::Unique,
                    ConstraintKindChoice::Foreign,
                    ConstraintKindChoice::Check,
                ] {
                    ui.selectable_value(&mut new.kind, kind, kind.label());
                }
            });
        if new.kind == ConstraintKindChoice::Check {
            ui.add(
                egui::TextEdit::singleline(&mut new.condition)
                    .hint_text("condition, e.g. salary > 0")
                    .desired_width(300.0),
            );
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut new.columns)
                    .hint_text("columns, e.g. a, b")
                    .desired_width(200.0),
            );
            if let Some(c) = picker(ui, "constraint-column", &columns) {
                append(&mut new.columns, &c);
            }
        }
        if new.kind == ConstraintKindChoice::Foreign {
            ui.label("references");
            ui.add(
                egui::TextEdit::singleline(&mut new.ref_table)
                    .hint_text("table")
                    .desired_width(160.0),
            );
            if let Some(t) = picker(ui, "constraint-table", &lists.tables) {
                new.ref_table = quote_ident(&t);
                new.ref_columns = lists
                    .keys
                    .get(&t)
                    .map(|k| typed_list(k))
                    .unwrap_or_default();
            }
            let referenced = ident(&new.ref_table);
            if !referenced.is_empty() && !lists.keys.contains_key(&referenced) {
                lists.want_keys = Some(referenced.clone());
            }
            ui.add(
                egui::TextEdit::singleline(&mut new.ref_columns)
                    .hint_text("its key columns")
                    .desired_width(160.0),
            );
            egui::ComboBox::from_id_salt("new-constraint-delete")
                .selected_text(match new.on_delete {
                    Some(rule) => format!("on delete {}", rule.to_lowercase()),
                    None => "on delete: no action".to_owned(),
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut new.on_delete, None, "no action");
                    ui.selectable_value(&mut new.on_delete, Some("CASCADE"), "cascade")
                        .on_hover_text("Deleting the referenced row deletes these rows");
                    ui.selectable_value(&mut new.on_delete, Some("SET NULL"), "set null")
                        .on_hover_text("Deleting the referenced row clears the columns");
                });
        }
        let complete = match new.kind {
            ConstraintKindChoice::Check => !new.condition.trim().is_empty(),
            ConstraintKindChoice::Foreign => {
                !new.columns.trim().is_empty() && !new.ref_table.trim().is_empty()
            }
            _ => !new.columns.trim().is_empty(),
        };
        if ui.add_enabled(complete, egui::Button::new("Add")).clicked() {
            let (kind, columns, condition, references) = match new.kind {
                ConstraintKindChoice::Check => (
                    ConstraintKind::Check,
                    Vec::new(),
                    Some(new.condition.trim().to_owned()),
                    None,
                ),
                ConstraintKindChoice::Primary => (
                    ConstraintKind::Primary,
                    split_columns(&new.columns),
                    None,
                    None,
                ),
                ConstraintKindChoice::Unique => (
                    ConstraintKind::Unique,
                    split_columns(&new.columns),
                    None,
                    None,
                ),
                ConstraintKindChoice::Foreign => (
                    ConstraintKind::Foreign,
                    split_columns(&new.columns),
                    None,
                    Some((
                        new.ref_table.trim().to_owned(),
                        split_columns(&new.ref_columns),
                    )),
                ),
            };
            let on_delete = (kind == ConstraintKind::Foreign)
                .then(|| new.on_delete.map(str::to_owned))
                .flatten();
            table.constraints.push(ConstraintDef {
                name: new.name.trim().to_owned(),
                kind,
                columns,
                references,
                on_delete,
                condition,
                generated: false,
            });
            *new = NewConstraint {
                kind: new.kind,
                ..NewConstraint::default()
            };
        }
    });
    ui.label(
        RichText::new("Columns are part of the primary key on the Columns tab as well.")
            .small()
            .weak(),
    );
}

fn indexes_ui(ui: &mut Ui, table: &mut TableDef, new: &mut NewIndex) {
    let mut remove = None;
    egui::Grid::new("table-indexes")
        .striped(true)
        .num_columns(4)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            for title in ["Name", "Unique", "Columns", ""] {
                ui.strong(title);
            }
            ui.end_row();
            for (i, idx) in table.indexes.iter().enumerate() {
                if idx.name.is_empty() {
                    ui.weak("automatic");
                } else {
                    ui.label(RichText::new(&idx.name).monospace());
                }
                ui.label(if idx.unique { "yes" } else { "" });
                ui.label(RichText::new(idx.columns.join(", ")).monospace());
                if crate::icons_ext::small_icon_button(ui, Icon::Trash, "Drop index").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some(i) = remove {
        table.indexes.remove(i);
    }
    ui.add_space(8.0);
    let columns: Vec<String> = table.columns.iter().map(|c| ident(&c.name)).collect();
    ui.horizontal(|ui| {
        ui.label("New");
        ui.add(
            egui::TextEdit::singleline(&mut new.name)
                .hint_text("name (automatic)")
                .desired_width(160.0),
        );
        ui.checkbox(&mut new.unique, "Unique");
        ui.add(
            egui::TextEdit::singleline(&mut new.columns)
                .hint_text("columns, e.g. last_name, first_name")
                .desired_width(300.0),
        );
        if let Some(c) = picker(ui, "index-column", &columns) {
            append(&mut new.columns, &c);
        }
        if ui
            .add_enabled(!new.columns.trim().is_empty(), egui::Button::new("Add"))
            .clicked()
        {
            table.indexes.push(IndexDef {
                name: new.name.trim().to_owned(),
                unique: new.unique,
                columns: split_columns(&new.columns),
                generated: false,
            });
            *new = NewIndex::default();
        }
    });
    let unindexed = unindexed_foreign_keys(table);
    if !unindexed.is_empty() {
        ui.add_space(6.0);
        if ui
            .button(format!("Index the foreign keys ({})", unindexed.len()))
            .on_hover_text(
                "Deleting or updating a referenced row locks the whole table \
                 when its foreign key has no index",
            )
            .clicked()
        {
            for columns in unindexed {
                table.indexes.push(IndexDef {
                    name: String::new(),
                    unique: false,
                    columns,
                    generated: false,
                });
            }
        }
    }
}

/// Columns of foreign keys that no index (or primary or unique key) starts
/// with.
fn unindexed_foreign_keys(table: &TableDef) -> Vec<Vec<String>> {
    let normalize = |names: &[String]| -> Vec<String> {
        names
            .iter()
            .map(|n| ident(n.trim().trim_end_matches(" desc").trim_end_matches(" DESC")))
            .collect()
    };
    let leading: Vec<Vec<String>> = table
        .indexes
        .iter()
        .map(|i| normalize(&i.columns))
        .chain(
            table
                .constraints
                .iter()
                .filter(|c| matches!(c.kind, ConstraintKind::Primary | ConstraintKind::Unique))
                .map(|c| normalize(&c.columns)),
        )
        .collect();
    table
        .constraints
        .iter()
        .filter(|c| c.kind == ConstraintKind::Foreign && !c.columns.is_empty())
        .filter(|c| {
            let fk = normalize(&c.columns);
            !leading.iter().any(|l| l.starts_with(&fk))
        })
        .map(|c| c.columns.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, data_type: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
            default: None,
            comment: None,
            identity: false,
            generation: Default::default(),
        }
    }

    fn loaded(editor: &mut TableEditor, table: Option<TableDef>) {
        editor.loaded(Loaded {
            table,
            partitions: None,
            release: (23, 0),
            tables: vec!["DEPT".into()],
            tablespaces: vec!["USERS".into()],
        });
    }

    #[test]
    fn script_from_edits() {
        let original = TableDef {
            name: "EMP".into(),
            columns: vec![ColumnDef {
                nullable: false,
                ..column("ID", "NUMBER(10)")
            }],
            comment: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
        };
        let mut editor = TableEditor::new(ObjectRef {
            owner: "HR".into(),
            name: "EMP".into(),
            object_type: ObjectType::Table,
        });
        loaded(&mut editor, Some(original));
        assert!(!editor.is_dirty());
        assert_eq!(editor.statements_for_test(), Ok(Vec::new()));
        editor.edit_for_test(|t| {
            t.columns.push(ColumnDef {
                comment: Some("Full name".into()),
                ..column("name", "VARCHAR2(30 CHAR)")
            });
            t.indexes.push(IndexDef {
                name: String::new(),
                unique: false,
                columns: vec!["name".into()],
                generated: false,
            });
        });
        assert!(editor.is_dirty());
        assert_eq!(
            editor.statements_for_test().unwrap(),
            [
                "alter table emp add (name VARCHAR2(30 CHAR))",
                "create index emp_ix on emp (name)",
                "comment on column emp.name is 'Full name'",
            ]
        );
        let (statements, _) = editor.statements().unwrap();
        assert!(
            editor
                .script(&statements)
                .starts_with("-- Changes to HR.EMP"),
        );
    }

    #[test]
    fn renames_columns() {
        let original = TableDef {
            name: "EMP".into(),
            columns: vec![
                ColumnDef {
                    nullable: false,
                    ..column("ID", "NUMBER(10)")
                },
                ColumnDef {
                    comment: Some("Shown".into()),
                    ..column("NAME", "VARCHAR2(30 CHAR)")
                },
            ],
            comment: None,
            constraints: vec![ConstraintDef {
                name: "EMP_PK".into(),
                kind: ConstraintKind::Primary,
                columns: vec!["ID".into()],
                references: None,
                on_delete: None,
                condition: None,
                generated: false,
            }],
            indexes: vec![IndexDef {
                name: "EMP_NAME_IX".into(),
                unique: false,
                columns: vec!["\"NAME\" desc".into()],
                generated: false,
            }],
        };
        let mut editor = TableEditor::new(ObjectRef {
            owner: "HR".into(),
            name: "EMP".into(),
            object_type: ObjectType::Table,
        });
        loaded(&mut editor, Some(original));
        // Renamed as in the grid (keys follow), one also changed.
        editor.edit_for_test(|t| {
            t.columns[0].name = "emp_id".into();
            rename_column(t, "ID", "emp_id");
            t.columns[1].name = "full_name".into();
            t.columns[1].data_type = "VARCHAR2(60 CHAR)".into();
        });
        assert_eq!(
            editor.statements_for_test().unwrap(),
            [
                "alter table emp rename column id to emp_id",
                "alter table emp rename column name to full_name",
                "alter table emp modify (full_name VARCHAR2(60 CHAR))",
            ]
        );
        // Swapping two names directly would clash on the way.
        editor.edit_for_test(|t| {
            t.columns[0].name = "name".into();
            t.columns[1].name = "id".into();
        });
        assert!(editor.statements_for_test().is_err());
    }

    #[test]
    fn builds_new_tables() {
        let mut editor = TableEditor::create("HR".into(), None);
        assert_eq!(editor.title(), "New table");
        loaded(&mut editor, None);
        assert!(!editor.is_dirty());
        assert_eq!(
            editor.statements_for_test(),
            Err("Enter the table's name".into())
        );
        editor.edit_for_test(|t| {
            t.name = "orders".into();
            t.columns[0].name = "order_id".into();
            t.columns[0].identity = true;
            t.columns.push(column("dept_id", "NUMBER(10)"));
            t.columns.push(ColumnDef {
                nullable: false,
                ..column("\"Note\"", "VARCHAR2(100 CHAR)")
            });
        });
        // The primary key followed the renamed column.
        editor.edit_for_test(|t| rename_column(t, "ID", "order_id"));
        editor.edit_for_test(|t| {
            t.constraints.push(ConstraintDef {
                name: String::new(),
                kind: ConstraintKind::Foreign,
                columns: vec!["dept_id".into()],
                references: Some(("dept".into(), vec!["id".into()])),
                on_delete: Some("CASCADE".into()),
                condition: None,
                generated: false,
            });
            let fks = unindexed_foreign_keys(t);
            assert_eq!(fks, [vec!["dept_id".to_owned()]]);
        });
        assert!(editor.is_dirty());
        editor.state.options.tablespace = "users".into();
        assert_eq!(
            editor.statements_for_test().unwrap(),
            [
                "create table orders (\n  order_id NUMBER(10) generated by default on null as identity,\n  \
                 dept_id NUMBER(10),\n  \"Note\" VARCHAR2(100 CHAR) not null,\n  \
                 constraint orders_pk primary key (order_id)\n)\ntablespace users",
                "alter table orders add constraint orders_fk foreign key (dept_id) references dept (id) on delete cascade",
            ]
        );
        let (statements, _) = editor.statements().unwrap();
        assert!(
            editor
                .script(&statements)
                .starts_with("-- Creates HR.ORDERS")
        );

        editor.edit_for_test(|t| t.columns[2].name = "dept_id".into());
        assert_eq!(
            editor.statements_for_test(),
            Err("Column DEPT_ID appears twice".into())
        );
        editor.edit_for_test(|t| {
            t.columns[2].name = "note".into();
            t.constraints[1].references = Some(("dept".into(), Vec::new()));
        });
        assert_eq!(
            editor.statements_for_test(),
            Err("A foreign key needs as many referenced columns as columns".into())
        );
    }

    #[test]
    fn copies_tables() {
        let original = TableDef {
            name: "EMP".into(),
            columns: vec![column("ID", "NUMBER"), column("Mixed", "DATE")],
            comment: Some("Staff".into()),
            constraints: vec![ConstraintDef {
                name: "EMP_PK".into(),
                kind: ConstraintKind::Primary,
                columns: vec!["ID".into()],
                references: None,
                on_delete: None,
                condition: None,
                generated: false,
            }],
            indexes: vec![IndexDef {
                name: "EMP_MIXED_IX".into(),
                unique: false,
                columns: vec!["\"Mixed\"".into()],
                generated: false,
            }],
        };
        let copy = copy_of(&original);
        assert_eq!(copy.name, "");
        assert_eq!(copy.columns[1].name, "\"Mixed\"");
        assert_eq!(copy.constraints[0].name, "");
        assert_eq!(copy.indexes[0].name, "");
        let mut def = to_dictionary(&copy);
        def.name = "STAFF".into();
        let def = named(def, 30);
        assert_eq!(def.columns[1].name, "Mixed");
        assert_eq!(def.constraints[0].name, "STAFF_PK");
        assert_eq!(def.indexes[0].name, "STAFF_IX");
        // Round trip: typed and back is the table as loaded.
        assert_eq!(to_dictionary(&to_typed(&original)), original);
    }

    #[test]
    fn names_fit() {
        let mut def = new_table();
        def.name = "A_VERY_LONG_TABLE_NAME_OF_30_C".into();
        def.indexes.push(IndexDef {
            name: String::new(),
            unique: false,
            columns: vec!["ID".into()],
            generated: false,
        });
        def.indexes.push(IndexDef {
            name: String::new(),
            unique: false,
            columns: vec!["ID".into()],
            generated: false,
        });
        let def = named(def, 30);
        assert_eq!(def.constraints[0].name, "A_VERY_LONG_TABLE_NAME_OF_3_PK");
        assert_eq!(def.indexes[0].name, "A_VERY_LONG_TABLE_NAME_OF_3_IX");
        assert_eq!(def.indexes[1].name, "A_VERY_LONG_TABLE_NAME_OF_IX2");
        assert!(def.indexes.iter().all(|i| i.name.len() <= 30));
    }

    #[test]
    fn primary_key_from_columns() {
        let mut def = new_table();
        def.columns.push(column("CODE", "VARCHAR2(10)"));
        set_primary(&mut def, "code", true);
        assert_eq!(primary_key(&def).unwrap().columns, ["ID", "code"]);
        set_primary(&mut def, "ID", false);
        set_primary(&mut def, "CODE", false);
        assert!(primary_key(&def).is_none());
    }

    #[test]
    fn identity_needs_a_number_and_12c() {
        let mut def = new_table();
        def.name = "T".into();
        def.columns[0].identity = true;
        assert!(validate(&def, true, (12, 1)).is_ok());
        assert!(validate(&def, true, (11, 2)).is_err());
        def.columns[0].data_type = "VARCHAR2(10)".into();
        assert!(validate(&def, true, (19, 0)).is_err());
    }
    /// A table built in the form, created, reloaded (nothing left to
    /// change), changed, and copied.
    #[test]
    fn builds_live_tables() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let cleanup = "begin
               for t in (select table_name from user_tables
                          where table_name like 'PYTHIA\\_TB\\_%' escape '\\'
                          order by table_name desc) loop
                 execute immediate 'drop table ' || t.table_name || ' cascade constraints purge';
               end loop;
             end;";
        conn.execute(cleanup, &[]).unwrap();
        conn.execute(
            "create table pythia_tb_dept (id number(10) primary key, name varchar2(30))",
            &[],
        )
        .unwrap();
        let release = pythia_db::compat::release(&conn).unwrap();
        let lists = |table: Option<TableDef>| Loaded {
            table,
            partitions: None,
            release,
            tables: Vec::new(),
            tablespaces: Vec::new(),
        };
        let load_table = |name: &str| {
            table::load(&conn, &owner, Some(name))
                .unwrap()
                .remove(name)
                .unwrap()
        };

        let mut editor = TableEditor::create(owner.clone(), None);
        editor.loaded(lists(None));
        editor.edit_for_test(|t| {
            t.name = "pythia_tb_emp".into();
            t.columns[0].identity = release >= (12, 1);
            t.columns.push(column("dept_id", "NUMBER(10)"));
            t.columns.push(ColumnDef {
                nullable: false,
                default: Some("sysdate".into()),
                comment: Some("Hired".into()),
                ..column("hired", "DATE")
            });
            t.comment = Some("Staff".into());
            t.constraints.push(ConstraintDef {
                name: String::new(),
                kind: ConstraintKind::Foreign,
                columns: vec!["dept_id".into()],
                references: Some(("pythia_tb_dept".into(), vec!["id".into()])),
                on_delete: Some("SET NULL".into()),
                condition: None,
                generated: false,
            });
            t.constraints.push(ConstraintDef {
                name: String::new(),
                kind: ConstraintKind::Check,
                columns: Vec::new(),
                references: None,
                on_delete: None,
                condition: Some("dept_id > 0".into()),
                generated: false,
            });
            for columns in unindexed_foreign_keys(t) {
                t.indexes.push(IndexDef {
                    name: String::new(),
                    unique: false,
                    columns,
                    generated: false,
                });
            }
        });
        let sql = editor.statements_for_test().unwrap();
        run(&conn, &owner, &sql).unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));

        // Reloaded, it's what the form said: nothing left to apply.
        let emp = load_table("PYTHIA_TB_EMP");
        assert_eq!(emp.comment.as_deref(), Some("Staff"));
        assert_eq!(emp.columns[0].identity, release >= (12, 1));
        assert_eq!(emp.indexes[0].name, "PYTHIA_TB_EMP_IX");
        let names: Vec<&str> = emp.constraints.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["PYTHIA_TB_EMP_CK", "PYTHIA_TB_EMP_FK", "PYTHIA_TB_EMP_PK"]
        );
        let object = ObjectRef {
            owner: owner.clone(),
            name: "PYTHIA_TB_EMP".into(),
            object_type: ObjectType::Table,
        };
        let mut again = TableEditor::new(object.clone());
        again.loaded(lists(Some(emp.clone())));
        assert_eq!(again.statements_for_test(), Ok(Vec::new()));

        // Changed later: a column with a unique key.
        again.edit_for_test(|t| {
            t.columns.push(column("badge", "VARCHAR2(10 CHAR)"));
            t.constraints.push(ConstraintDef {
                name: String::new(),
                kind: ConstraintKind::Unique,
                columns: vec!["badge".into()],
                references: None,
                on_delete: None,
                condition: None,
                generated: false,
            });
        });
        let sql = again.statements_for_test().unwrap();
        assert_eq!(sql.len(), 2, "{sql:?}");
        run(&conn, &owner, &sql).unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));
        let emp = load_table("PYTHIA_TB_EMP");
        let mut reloaded = TableEditor::new(object.clone());
        reloaded.loaded(lists(Some(emp.clone())));
        assert_eq!(reloaded.statements_for_test(), Ok(Vec::new()));

        // Renaming keeps the data, the unique key and the index.
        // Before 12.1 the id isn't an identity column.
        let insert = if release >= (12, 1) {
            "insert into pythia_tb_emp (dept_id, hired, badge) values (null, sysdate, 'B-1')"
        } else {
            "insert into pythia_tb_emp (id, dept_id, hired, badge) values (1, null, sysdate, 'B-1')"
        };
        conn.execute(insert, &[]).unwrap();
        conn.commit().unwrap();
        reloaded.edit_for_test(|t| {
            let i = t.columns.iter().position(|c| c.name == "BADGE").unwrap();
            t.columns[i].name = "badge_no".into();
            rename_column(t, "BADGE", "badge_no");
        });
        let sql = reloaded.statements_for_test().unwrap();
        assert_eq!(
            sql,
            ["alter table pythia_tb_emp rename column badge to badge_no"]
        );
        run(&conn, &owner, &sql).unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));
        let badge: String = conn
            .query_row_as("select badge_no from pythia_tb_emp", &[])
            .unwrap();
        assert_eq!(badge, "B-1");
        let emp = load_table("PYTHIA_TB_EMP");
        let mut reloaded = TableEditor::new(object.clone());
        reloaded.loaded(lists(Some(emp.clone())));
        assert_eq!(reloaded.statements_for_test(), Ok(Vec::new()));
        // A column with a foreign key and an index: both follow.
        reloaded.edit_for_test(|t| {
            let i = t.columns.iter().position(|c| c.name == "DEPT_ID").unwrap();
            t.columns[i].name = "department_id".into();
            rename_column(t, "DEPT_ID", "department_id");
        });
        let sql = reloaded.statements_for_test().unwrap();
        assert_eq!(
            sql,
            ["alter table pythia_tb_emp rename column dept_id to department_id"]
        );
        run(&conn, &owner, &sql).unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));
        let emp = load_table("PYTHIA_TB_EMP");
        let mut reloaded = TableEditor::new(object.clone());
        reloaded.loaded(lists(Some(emp.clone())));
        assert_eq!(reloaded.statements_for_test(), Ok(Vec::new()));

        // A table like it, with names of its own.
        let mut like = TableEditor::create(owner.clone(), Some(object));
        like.loaded(lists(Some(emp)));
        like.edit_for_test(|t| t.name = "pythia_tb_emp2".into());
        let sql = like.statements_for_test().unwrap();
        run(&conn, &owner, &sql).unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));
        let copy = load_table("PYTHIA_TB_EMP2");
        let names: Vec<&str> = copy.constraints.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "PYTHIA_TB_EMP2_CK",
                "PYTHIA_TB_EMP2_FK",
                "PYTHIA_TB_EMP2_PK",
                "PYTHIA_TB_EMP2_UK"
            ]
        );
        assert_eq!(copy.columns.len(), 4);
        assert_eq!(copy.indexes.len(), 1);
        conn.execute(cleanup, &[]).unwrap();
    }

    /// Partitioned tables through the editor: range, list, hash and interval
    /// tables created, reloaded, partitions added, truncated and dropped.
    #[test]
    fn builds_live_partitioned_tables() {
        use pythia_compare::table::{PartitionDef, PartitionMethod, Partitioning};
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let cleanup = "begin
               for t in (select table_name from user_tables
                          where table_name like 'PYTHIA\\_TP\\_%' escape '\\') loop
                 execute immediate 'drop table ' || t.table_name || ' purge';
               end loop;
             end;";
        conn.execute(cleanup, &[]).unwrap();
        let release = pythia_db::compat::release(&conn).unwrap();
        let part = |name: &str, bound: &str| PartitionDef {
            name: name.into(),
            bound: bound.into(),
            tablespace: String::new(),
        };
        let build = |name: &str, p: Partitioning, key: &str| -> Result<(), ApplyError> {
            let mut editor = TableEditor::create(owner.clone(), None);
            editor.loaded(Loaded {
                table: None,
                partitions: None,
                release,
                tables: Vec::new(),
                tablespaces: Vec::new(),
            });
            editor.edit_for_test(|t| {
                t.name = name.into();
                t.columns.push(column("created", "DATE"));
                t.columns.push(column("region", "VARCHAR2(10 CHAR)"));
            });
            editor.state.partitioning = Some(p);
            editor.state.partition_key = key.into();
            let sql = editor.statements_for_test().unwrap();
            run(&conn, &owner, &sql)
        };

        let range = Partitioning {
            method: PartitionMethod::Range,
            partitions: vec![
                part("p_2025", "DATE '2026-01-01'"),
                part("p_max", "MAXVALUE"),
            ],
            ..Partitioning::default()
        };
        match build("pythia_tp_range", range, "created") {
            Err(e) if e.error.code == Some(439) => {
                eprintln!("no Partitioning option, skipping: {}", e.error);
                return;
            }
            other => other.unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error)),
        }
        let list = Partitioning {
            method: PartitionMethod::List,
            partitions: vec![part("p_eu", "'CH', 'DE'"), part("p_rest", "DEFAULT")],
            ..Partitioning::default()
        };
        build("pythia_tp_list", list, "region")
            .unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));
        let hash = Partitioning {
            method: PartitionMethod::Hash,
            hash_count: 4,
            ..Partitioning::default()
        };
        build("pythia_tp_hash", hash, "id")
            .unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));
        let interval = Partitioning {
            method: PartitionMethod::Interval,
            interval: "NUMTOYMINTERVAL(1, 'MONTH')".into(),
            partitions: vec![part("p_start", "DATE '2026-01-01'")],
            ..Partitioning::default()
        };
        build("pythia_tp_interval", interval, "created")
            .unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));

        // Reloaded: kinds, keys and partitions.
        let range = table_partitions::load(&conn, &owner, "PYTHIA_TP_RANGE")
            .unwrap()
            .unwrap();
        assert_eq!(range.method, Some(PartitionMethod::Range));
        assert_eq!(range.columns, ["CREATED"]);
        let names: Vec<&str> = range
            .partitions
            .iter()
            .map(|(p, _)| p.name.as_str())
            .collect();
        assert_eq!(names, ["P_2025", "P_MAX"]);
        assert_eq!(range.partitions[1].0.bound, "MAXVALUE");
        let list = table_partitions::load(&conn, &owner, "PYTHIA_TP_LIST")
            .unwrap()
            .unwrap();
        assert_eq!(list.partitions[0].0.bound, "'CH', 'DE'");
        let hash = table_partitions::load(&conn, &owner, "PYTHIA_TP_HASH")
            .unwrap()
            .unwrap();
        assert_eq!(hash.partitions.len(), 4);
        // A row past the interval table's first partition adds one.
        conn.execute(
            "insert into pythia_tp_interval (id, created) values (1, date '2026-03-15')",
            &[],
        )
        .unwrap();
        conn.commit().unwrap();
        let interval = table_partitions::load(&conn, &owner, "PYTHIA_TP_INTERVAL")
            .unwrap()
            .unwrap();
        assert_eq!(interval.method, Some(PartitionMethod::Interval));
        assert_eq!(interval.partitions.len(), 2);
        assert!(interval.partitions[1].1, "added by the interval");
        assert!(
            table_partitions::load(&conn, &owner, "PYTHIA_TP_NONE")
                .unwrap()
                .is_none()
        );

        // An existing list table: add a partition, truncate and drop others.
        let object = ObjectRef {
            owner: owner.clone(),
            name: "PYTHIA_TP_LIST".into(),
            object_type: ObjectType::Table,
        };
        let mut editor = TableEditor::new(object);
        let table = table::load(&conn, &owner, Some("PYTHIA_TP_LIST"))
            .unwrap()
            .remove("PYTHIA_TP_LIST")
            .unwrap();
        editor.loaded(Loaded {
            table: Some(table),
            partitions: Some(list),
            release,
            tables: Vec::new(),
            tablespaces: Vec::new(),
        });
        assert!(!editor.is_dirty());
        let p = editor.state.partitions.as_mut().unwrap();
        p.added.push(part("p_us", "'US'"));
        p.actions
            .insert("P_EU".into(), table_partitions::Action::Truncate);
        p.actions
            .insert("P_REST".into(), table_partitions::Action::Drop);
        assert!(editor.is_dirty());
        // Without "allow dropping" only the new partition runs.
        assert_eq!(
            editor.statements_for_test().unwrap(),
            ["alter table pythia_tp_list add partition p_us values ('US')"]
        );
        editor.state.allow_drops = true;
        let sql = editor.statements_for_test().unwrap();
        assert_eq!(sql.len(), 3, "{sql:?}");
        run(&conn, &owner, &sql).unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));
        let list = table_partitions::load(&conn, &owner, "PYTHIA_TP_LIST")
            .unwrap()
            .unwrap();
        let names: Vec<&str> = list
            .partitions
            .iter()
            .map(|(p, _)| p.name.as_str())
            .collect();
        assert_eq!(names, ["P_EU", "P_US"]);

        // A copy of the interval table: the same partitioning, without the
        // partition the interval added.
        let mut like = TableEditor::create(
            owner.clone(),
            Some(ObjectRef {
                owner: owner.clone(),
                name: "PYTHIA_TP_INTERVAL".into(),
                object_type: ObjectType::Table,
            }),
        );
        let source = table::load(&conn, &owner, Some("PYTHIA_TP_INTERVAL"))
            .unwrap()
            .remove("PYTHIA_TP_INTERVAL")
            .unwrap();
        like.loaded(Loaded {
            table: Some(source),
            partitions: Some(interval),
            release,
            tables: Vec::new(),
            tablespaces: Vec::new(),
        });
        like.edit_for_test(|t| t.name = "pythia_tp_interval2".into());
        let sql = like.statements_for_test().unwrap();
        assert!(
            sql[0].contains("interval (NUMTOYMINTERVAL(1, 'MONTH'))"),
            "{}",
            sql[0]
        );
        assert!(!sql[0].contains("SYS_P"), "{}", sql[0]);
        run(&conn, &owner, &sql).unwrap_or_else(|e| panic!("{}: {}", e.statement, e.error));
        conn.execute(cleanup, &[]).unwrap();
    }
}
