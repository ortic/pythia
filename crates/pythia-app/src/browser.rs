//! Object browser: connection → schema → object type → objects → children.
//!
//! The tree is flattened into visible rows every frame and drawn with a
//! virtualized scroll area, so folders with tens of thousands of objects stay
//! smooth. Folders load lazily when expanded.

use std::collections::{HashMap, HashSet};

use egui::{Color32, RichText, Sense, Ui};
use pythia_db::{
    DbEvent, DbObject, DbRequest, Dialect, Feature, Member, MetaQuery, MetaResult, ObjectType,
    TableColumn,
};

use pythia_ui::icons::{Icon, IconSize, paint_invalid_badge};

use crate::documents::{Effects, ObjectRef, OpenRequest, Target, quote_ident, request_id};
use crate::object_editor::{EditTarget, can_create, has_editor, has_form, has_grants};
use crate::table_editor::TableTab;
use crate::table_parts::{Part, PartKind, TableParts};

enum Loaded<T> {
    Loading,
    Ready(T),
    Failed(String),
}

enum Children {
    Columns(Vec<TableColumn>),
    Members(Vec<Member>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Node {
    Folder(String, ObjectType),
    Object(ObjectRef),
    /// Indexes, constraints or triggers of a table.
    Group(ObjectRef, Group),
    /// Favorites or recently opened objects.
    Section(Section),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Section {
    Favorites,
    Recent,
    Users,
    Roles,
    Profiles,
    Directories,
}

impl Section {
    /// Sections listing the database's own objects, in display order.
    const DATABASE: [Section; 4] = [
        Section::Users,
        Section::Roles,
        Section::Profiles,
        Section::Directories,
    ];

    /// The users, roles, profiles or directories a section lists.
    fn accounts(self) -> Option<ObjectType> {
        match self {
            Section::Users => Some(ObjectType::User),
            Section::Roles => Some(ObjectType::Role),
            Section::Profiles => Some(ObjectType::Profile),
            Section::Directories => Some(ObjectType::Directory),
            Section::Favorites | Section::Recent => None,
        }
    }

    /// The section listing objects of a type.
    fn listing(object_type: ObjectType) -> Option<Section> {
        Section::DATABASE
            .into_iter()
            .find(|s| s.accounts() == Some(object_type))
    }
}

/// Favorite and recently opened objects per connection, and the database
/// last picked; saved with the workspace.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Marks {
    pub favorites: std::collections::BTreeMap<String, Vec<ObjectRef>>,
    pub recent: std::collections::BTreeMap<String, Vec<ObjectRef>>,
    /// MySQL: the browser opens it again when the connection comes up
    /// without a database of its own (#133).
    #[serde(default)]
    pub databases: std::collections::BTreeMap<String, String>,
}

impl Marks {
    const RECENT: usize = 12;

    /// Remembers an object as just opened.
    pub fn touch(&mut self, connection: &str, object: ObjectRef) {
        let list = self.recent.entry(connection.to_owned()).or_default();
        list.retain(|o| *o != object);
        list.insert(0, object);
        list.truncate(Self::RECENT);
    }

    fn toggle_favorite(&mut self, connection: &str, object: ObjectRef) {
        let list = self.favorites.entry(connection.to_owned()).or_default();
        if let Some(i) = list.iter().position(|o| *o == object) {
            list.remove(i);
        } else {
            list.push(object);
            list.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    pub fn is_favorite(&self, connection: &str, object: &ObjectRef) -> bool {
        self.favorites
            .get(connection)
            .is_some_and(|l| l.contains(object))
    }
}

/// Which objects the folders list besides the name filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Changed {
    #[default]
    Any,
    Day,
    Week,
    Month,
}

impl Changed {
    fn days(self) -> Option<i64> {
        match self {
            Changed::Any => None,
            Changed::Day => Some(1),
            Changed::Week => Some(7),
            Changed::Month => Some(30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Group {
    Indexes,
    Constraints,
    Triggers,
    ReferencedBy,
}

impl Group {
    fn label(self) -> &'static str {
        match self {
            Group::Indexes => "Indexes",
            Group::Constraints => "Constraints",
            Group::Triggers => "Triggers",
            Group::ReferencedBy => "Referenced by",
        }
    }
}

enum Pending {
    Schemas,
    Folder(String, ObjectType),
    Children(ObjectRef),
    /// Indexes, constraints and triggers of a table.
    Parts(ObjectRef),
    Action {
        object: ObjectRef,
        verb: &'static str,
    },
    /// Columns for a generated statement.
    Generate {
        object: ObjectRef,
        kind: Template,
    },
    /// A test script of a subprogram of `object` (or the standalone
    /// unit), to debug or not.
    Test {
        object: ObjectRef,
        debug: bool,
    },
    TestLink(ObjectRef),
    /// DDL for a script that replaces the object.
    Script(ObjectRef),
    /// Users or roles of the database.
    Accounts(ObjectType),
}

/// The folders of a schema; Oracle's `PUBLIC` only owns synonyms and links.
fn folders(dialect: Dialect, mariadb: bool, owner: &str) -> &'static [ObjectType] {
    if owner == "PUBLIC" && dialect == Dialect::ORACLE {
        ObjectType::PUBLIC
    } else {
        browser_types(dialect, mariadb)
    }
}

/// The folders of the database's schemas: MariaDB has sequences, which
/// MySQL doesn't (#270).
fn browser_types(dialect: Dialect, mariadb: bool) -> &'static [ObjectType] {
    const MARIADB: &[ObjectType] = &[
        ObjectType::Table,
        ObjectType::View,
        ObjectType::Procedure,
        ObjectType::Function,
        ObjectType::Trigger,
        ObjectType::Sequence,
    ];
    if mariadb && dialect == Dialect::MYSQL {
        MARIADB
    } else {
        dialect.browser_types()
    }
}

/// Whether a name contains the (uppercase) filter, without case: on Oracle
/// too, where a quoted name keeps its lowercase letters (#305).
pub(crate) fn name_matches(name: &str, filter: &str) -> bool {
    filter.is_empty() || name.to_uppercase().contains(filter)
}

/// One visible line of the tree.
enum Row<'a> {
    Folder {
        owner: &'a str,
        object_type: ObjectType,
        count: Option<usize>,
        open: bool,
    },
    Object {
        object: &'a DbObject,
        open: bool,
        expandable: bool,
    },
    /// A table column; whether it's part of a foreign key.
    Column(&'a TableColumn, bool),
    Group {
        table: ObjectRef,
        group: Group,
        count: usize,
        open: bool,
    },
    Part(&'a Part, ObjectRef),
    Member(&'a Member, ObjectRef),
    Section {
        section: Section,
        /// Unknown until loaded (users and roles).
        count: Option<usize>,
        open: bool,
    },
    /// An object listed under Favorites or Recent.
    Shortcut(&'a ObjectRef),
    /// A user, role, profile or directory: name, and what else to show
    /// (a directory's path).
    Account(&'a str, &'a str, ObjectType),
    Message {
        depth: usize,
        text: String,
        error: bool,
    },
}

#[derive(Default)]
pub struct Browser {
    pub connection: Option<String>,
    /// The connection's database: its folders and what objects offer.
    dialect: Dialect,
    /// The server is MariaDB, which replaces objects in place.
    mariadb: bool,
    schema: Option<String>,
    schemas: Option<Loaded<Vec<String>>>,
    filter: String,
    invalid_only: bool,
    changed: Changed,
    pub marks: Marks,
    folders: HashMap<(String, ObjectType), Loaded<Vec<DbObject>>>,
    /// Users, roles, profiles and directories by type: name and detail.
    accounts: HashMap<ObjectType, Loaded<Vec<(String, String)>>>,
    children: HashMap<ObjectRef, Loaded<Children>>,
    parts: HashMap<ObjectRef, TableParts>,
    expanded: HashSet<Node>,
    pending: HashMap<u64, Pending>,
    selected: Option<ObjectRef>,
    /// Scroll to this object once its row exists.
    reveal: Option<ObjectRef>,
    confirm_drop: Option<ObjectRef>,
}

impl Browser {
    /// Shows a (newly connected) connection, starting in its current schema.
    pub fn attach(
        &mut self,
        connection: String,
        schema: String,
        dialect: Dialect,
        mariadb: bool,
    ) -> Effects {
        let marks = std::mem::take(&mut self.marks);
        *self = Self {
            connection: Some(connection),
            dialect,
            mariadb,
            schema: Some(schema),
            marks,
            ..Self::default()
        };
        self.load_schemas()
    }

    /// Stops showing the connection; favorites and recent objects stay
    /// (#136).
    pub fn detach(&mut self, connection: &str) {
        if self.connection.as_deref() == Some(connection) {
            let marks = std::mem::take(&mut self.marks);
            *self = Self {
                marks,
                ..Self::default()
            };
        }
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "used by screenshot tests"))]
    pub fn is_loading(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The database last picked, if the connection came up without one
    /// (MySQL): opened once the list shows it's still there. A database
    /// from the profile (address, default schema, login script) wins.
    fn last_database(&self) -> Option<&String> {
        if self.dialect != Dialect::MYSQL || self.schema.as_deref() != Some("") {
            return None;
        }
        self.marks.databases.get(self.connection.as_ref()?)
    }

    /// Opens the database last picked once the list is in, or forgets it
    /// when it's gone (dropped, or no longer granted).
    fn open_last_database(&mut self, effects: &mut Effects, log: &mut Vec<String>) {
        let (Some(database), Some(Loaded::Ready(list))) = (self.last_database(), &self.schemas)
        else {
            return;
        };
        let database = database.clone();
        if list.contains(&database) {
            self.set_schema(database, effects);
        } else if let Some(connection) = &self.connection {
            log.push(format!(
                "{connection}: database {database}, last used, isn't there anymore"
            ));
            self.marks.databases.remove(connection);
        }
    }

    /// Whether a user has a schema to show: on Oracle each user is one, on
    /// PostgreSQL one may have the user's name. MySQL's databases and SQL
    /// Server's schemas aren't users'.
    fn has_schema_of(&self, account: &ObjectRef) -> bool {
        if account.object_type != ObjectType::User {
            return false;
        }
        match self.dialect.kind() {
            pythia_db::DbKind::Oracle => true,
            pythia_db::DbKind::Postgres => matches!(
                &self.schemas,
                Some(Loaded::Ready(schemas)) if schemas.contains(&account.name)
            ),
            pythia_db::DbKind::MySql | pythia_db::DbKind::SqlServer | pythia_db::DbKind::Sqlite => {
                false
            }
        }
    }

    /// Shows another schema, keeping the same folders open.
    fn set_schema(&mut self, schema: String, effects: &mut Effects) {
        if self.schema.as_ref() == Some(&schema) {
            return;
        }
        // On MySQL the database picked here is the one statements run in,
        // and the one to start in next time.
        if self.dialect == Dialect::MYSQL {
            effects.database = Some(schema.clone());
            if let Some(connection) = &self.connection {
                self.marks
                    .databases
                    .insert(connection.clone(), schema.clone());
            }
        }
        self.schema = Some(schema);
        self.expanded.retain(|n| matches!(n, Node::Folder(..)));
        let folders: Vec<ObjectType> = self
            .expanded
            .drain()
            .filter_map(|n| {
                if let Node::Folder(_, t) = n {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();
        let owner = self.schema.clone().unwrap_or_default();
        for t in folders {
            self.expanded.insert(Node::Folder(owner.clone(), t));
        }
        self.refresh(effects);
    }

    /// Shows an object in the tree: in its schema, with its folder open,
    /// selected, and scrolled to once the folder has loaded. Bodies show as
    /// their package or type.
    fn reveal(&mut self, object: ObjectRef, effects: &mut Effects) {
        let object_type = match object.object_type {
            ObjectType::PackageBody => ObjectType::Package,
            ObjectType::TypeBody => ObjectType::Type,
            t => t,
        };
        if !browser_types(self.dialect, self.mariadb).contains(&object_type) {
            effects.log.push(format!(
                "{} objects aren't listed in the object browser",
                object_type.dictionary_name().to_lowercase()
            ));
            return;
        }
        let object = ObjectRef {
            object_type,
            ..object
        };
        self.set_schema(object.owner.clone(), effects);
        if !name_matches(&object.name, &self.filter.trim().to_uppercase()) {
            self.filter.clear();
        }
        let node = Node::Folder(object.owner.clone(), object_type);
        if self.expanded.insert(node)
            && !self
                .folders
                .contains_key(&(object.owner.clone(), object_type))
        {
            self.load_folder(effects, &object.owner, object_type);
        }
        self.selected = Some(object.clone());
        self.reveal = Some(object);
    }

    /// Expands the given folders of the current schema.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by screenshot tests"))]
    pub fn expand(&mut self, types: &[ObjectType]) -> Effects {
        let mut effects = Effects::default();
        let owner = self.schema.clone().unwrap_or_default();
        for t in types {
            if self.expanded.insert(Node::Folder(owner.clone(), *t)) {
                self.load_folder(&mut effects, &owner, *t);
            }
        }
        effects
    }

    /// Opens Favorites and Recent, marks `favorite` and filters invalid objects.
    #[cfg(test)]
    pub fn test_marks(&mut self, favorite: ObjectRef) {
        let connection = self.connection.clone().unwrap_or_default();
        self.marks.toggle_favorite(&connection, favorite);
        self.expanded.insert(Node::Section(Section::Favorites));
        self.expanded.insert(Node::Section(Section::Recent));
        self.invalid_only = true;
    }

    /// What the toolbar's Refresh does.
    #[cfg(test)]
    pub fn test_refresh(&mut self) -> Effects {
        let mut effects = Effects::default();
        self.refresh(&mut effects);
        effects
    }

    #[cfg(test)]
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_owned();
    }

    #[cfg(test)]
    pub fn selected(&self) -> Option<&ObjectRef> {
        self.selected.as_ref()
    }

    /// Expands a table with all its groups.
    #[cfg(test)]
    pub fn expand_table(&mut self, table: ObjectRef) -> Effects {
        let mut effects = Effects::default();
        self.load_children(&mut effects, &table);
        for group in [
            Group::Indexes,
            Group::Constraints,
            Group::Triggers,
            Group::ReferencedBy,
        ] {
            self.expanded.insert(Node::Group(table.clone(), group));
        }
        self.expanded.insert(Node::Object(table));
        effects
    }

    /// The schema shown.
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    pub fn owns(&self, id: u64) -> bool {
        self.pending.contains_key(&id)
    }

    fn load_schemas(&mut self) -> Effects {
        let mut effects = Effects::default();
        self.schemas = Some(Loaded::Loading);
        self.meta(&mut effects, Pending::Schemas, MetaQuery::Schemas);
        effects
    }

    fn meta(&mut self, effects: &mut Effects, pending: Pending, query: MetaQuery) {
        let id = request_id();
        self.pending.insert(id, pending);
        effects.request(Target::Main, DbRequest::Meta { id, query });
    }

    fn load_folder(&mut self, effects: &mut Effects, owner: &str, object_type: ObjectType) {
        self.folders
            .insert((owner.to_owned(), object_type), Loaded::Loading);
        self.meta(
            effects,
            Pending::Folder(owner.to_owned(), object_type),
            MetaQuery::Objects {
                owner: owner.to_owned(),
                object_type,
                filter: None,
            },
        );
    }

    /// Loads the users, roles, profiles or directories of the database.
    fn load_accounts(&mut self, effects: &mut Effects, object_type: ObjectType) {
        self.accounts.insert(object_type, Loaded::Loading);
        self.meta(
            effects,
            Pending::Accounts(object_type),
            MetaQuery::Accounts(object_type),
        );
    }

    fn load_children(&mut self, effects: &mut Effects, object: &ObjectRef) {
        let query = match object.object_type {
            t if t.has_data() => MetaQuery::Columns {
                owner: object.owner.clone(),
                table: object.name.clone(),
                keys: t == ObjectType::Table,
            },
            ObjectType::Package | ObjectType::Type => MetaQuery::Members {
                owner: object.owner.clone(),
                package: object.name.clone(),
            },
            _ => return,
        };
        self.children.insert(object.clone(), Loaded::Loading);
        self.meta(effects, Pending::Children(object.clone()), query);
        if object.object_type == ObjectType::Table && self.dialect.supports(Feature::TableParts) {
            let id = request_id();
            self.pending.insert(id, Pending::Parts(object.clone()));
            let (owner, table) = (object.owner.clone(), object.name.clone());
            let task: pythia_db::TaskFn = Box::new(move |conn: &pythia_db::oracle::Connection| {
                let parts = crate::table_parts::load(conn, &owner, &table)?;
                Ok(Box::new(parts) as Box<dyn std::any::Any + Send>)
            });
            effects.request(Target::Main, DbRequest::Task { id, task });
        }
    }

    /// Reloads expanded folders (after compile/drop or on demand).
    fn refresh(&mut self, effects: &mut Effects) {
        effects.catalog_changed = true;
        self.children.clear();
        self.parts.clear();
        let open: Vec<(String, ObjectType)> = self
            .expanded
            .iter()
            .filter_map(|n| match n {
                Node::Folder(owner, t) => Some((owner.clone(), *t)),
                Node::Object(_) | Node::Group(..) | Node::Section(_) => None,
            })
            .collect();
        self.folders.clear();
        for (owner, t) in open {
            self.load_folder(effects, &owner, t);
        }
        let objects: Vec<ObjectRef> = self
            .expanded
            .iter()
            .filter_map(|n| match n {
                Node::Object(o) => Some(o.clone()),
                Node::Folder(..) | Node::Group(..) | Node::Section(_) => None,
            })
            .collect();
        for o in objects {
            self.load_children(effects, &o);
        }
        let accounts: Vec<ObjectType> = self.accounts.keys().copied().collect();
        for t in accounts {
            self.load_accounts(effects, t);
        }
    }

    /// An object was created or changed elsewhere (in an editor): lists it
    /// as it is now and shows it.
    pub fn show_changed(&mut self, object: ObjectRef) -> Effects {
        let mut effects = Effects::default();
        match Section::listing(object.object_type) {
            Some(section) => {
                self.load_accounts(&mut effects, object.object_type);
                self.expanded.insert(Node::Section(section));
            }
            None => {
                self.refresh(&mut effects);
                self.reveal(object, &mut effects);
            }
        }
        effects
    }

    fn open_test(
        &self,
        script: crate::test_script::Script,
        object: &ObjectRef,
        debug: bool,
        effects: &mut Effects,
    ) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let text = script.block;
        effects.open.push(if debug {
            // A package's subprograms run in its body.
            let unit_type = match object.object_type {
                ObjectType::Package => ObjectType::PackageBody,
                t => t,
            };
            OpenRequest::Debug {
                connection,
                text,
                entry: (
                    crate::program_window::debug_unit(object, unit_type),
                    script.subprogram,
                ),
            }
        } else {
            OpenRequest::Sql {
                connection,
                text,
                run: false,
            }
        });
    }

    pub fn on_event(&mut self, event: DbEvent, log: &mut Vec<String>) -> Effects {
        let mut effects = Effects::default();
        if let DbEvent::TaskDone { id, result } = event {
            if let Some(Pending::Parts(object)) = self.pending.get(&id) {
                let object = object.clone();
                self.pending.remove(&id);
                // Without the parts the table still shows its columns.
                if let Ok(Ok(parts)) = result.map(|any| any.downcast::<TableParts>()) {
                    self.parts.insert(object, *parts);
                }
                return effects;
            }
            if let Some(Pending::Test { object, debug }) = self.pending.remove(&id) {
                match result.map(|any| any.downcast::<crate::test_script::Script>()) {
                    Ok(Ok(script)) => self.open_test(*script, &object, debug, &mut effects),
                    Ok(Err(_)) => {}
                    Err(e) => log.push(format!("Test script: {e}")),
                }
            }
            return effects;
        }
        if let DbEvent::Meta { id, result } = &event
            && let Some(Pending::Generate { object, kind }) = self.pending.get(id)
        {
            let (object, kind) = (object.clone(), *kind);
            self.pending.remove(id);
            match result {
                Ok(MetaResult::Columns(cols)) => {
                    effects.open.push(OpenRequest::InsertText {
                        text: template(self.dialect, kind, &object, cols),
                    });
                }
                Ok(_) => {}
                Err(e) => log.push(format!("Generate: {e}")),
            }
            return effects;
        }
        if let DbEvent::Meta { id, result } = &event
            && let Some(Pending::Script(object)) = self.pending.get(id)
        {
            let object = object.clone();
            self.pending.remove(id);
            match (result, self.connection.clone()) {
                (Ok(MetaResult::Text(ddl)), Some(connection)) => {
                    effects.open.push(OpenRequest::Sql {
                        connection,
                        text: replace_script(self.dialect, &object, ddl, self.mariadb),
                        run: false,
                    });
                }
                (Err(e), _) => log.push(format!("Edit {}: {e}", object.name)),
                _ => {}
            }
            return effects;
        }
        let (id, result) = match event {
            DbEvent::Meta { id, result } => (id, result.map(Some)),
            DbEvent::Executed { id, .. } => (id, Ok(None)),
            DbEvent::Failed { id, error, .. } => (id, Err(error)),
            _ => return effects,
        };
        let Some(pending) = self.pending.remove(&id) else {
            return effects;
        };
        match (pending, result) {
            (Pending::Schemas, Ok(Some(MetaResult::Schemas(mut s)))) => {
                // Oracle's public synonyms and links belong to PUBLIC, which
                // isn't a user (ALL_USERS is in binary order).
                if self.dialect == Dialect::ORACLE
                    && let Err(at) = s.binary_search_by(|u| u.as_str().cmp("PUBLIC"))
                {
                    s.insert(at, "PUBLIC".into());
                }
                self.schemas = Some(Loaded::Ready(s));
                self.open_last_database(&mut effects, log);
            }
            (Pending::Schemas, Err(e)) => self.schemas = Some(Loaded::Failed(e.to_string())),
            (Pending::Folder(owner, t), Ok(Some(MetaResult::Objects(objects)))) => {
                self.folders.insert((owner, t), Loaded::Ready(objects));
            }
            (Pending::Folder(owner, t), Err(e)) => {
                self.folders
                    .insert((owner, t), Loaded::Failed(e.to_string()));
            }
            (Pending::Children(o), Ok(Some(MetaResult::Columns(c)))) => {
                self.children.insert(o, Loaded::Ready(Children::Columns(c)));
            }
            (Pending::Children(o), Ok(Some(MetaResult::Members(m)))) => {
                self.children.insert(o, Loaded::Ready(Children::Members(m)));
            }
            (Pending::Children(o), Err(e)) => {
                self.children.insert(o, Loaded::Failed(e.to_string()));
            }
            (Pending::Accounts(t), Ok(Some(MetaResult::Accounts(names)))) => {
                self.accounts.insert(t, Loaded::Ready(names));
            }
            (Pending::Accounts(t), Err(e)) => {
                self.accounts.insert(t, Loaded::Failed(e.to_string()));
            }
            (Pending::Action { object, verb }, Ok(_)) => {
                let ty = object.object_type.dictionary_name().to_lowercase();
                log.push(format!("{ty} {} {verb}", named(self.dialect, &object)));
                self.refresh(&mut effects);
                effects.invalid_changed = true;
                if verb == "dropped" {
                    effects.dropped = Some(object);
                }
            }
            (Pending::Action { object, verb }, Err(e)) => {
                log.push(format!(
                    "{} {} failed: {e}",
                    verb,
                    named(self.dialect, &object)
                ));
                self.refresh(&mut effects);
            }
            (Pending::TestLink(object), Ok(Some(MetaResult::Text(remote)))) => {
                log.push(format!("Database link {} works: {remote}", object.name));
            }
            (Pending::TestLink(object), Err(e)) => {
                log.push(format!("Database link {} failed: {e}", object.name));
            }
            _ => {}
        }
        effects
    }

    fn run(&mut self, effects: &mut Effects, object: &ObjectRef, verb: &'static str, sql: String) {
        let id = request_id();
        self.pending.insert(
            id,
            Pending::Action {
                object: object.clone(),
                verb,
            },
        );
        effects.request(
            Target::Main,
            DbRequest::Execute {
                id,
                sql,
                max_rows: 1,
                binds: Vec::new(),
            },
        );
    }

    pub fn ui(&mut self, ui: &mut Ui, connections: &[String]) -> Effects {
        let mut effects = Effects::default();
        let Some(connection) = self.connection.clone() else {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.weak("Not connected");
                ui.add_space(4.0);
                ui.weak("Connect to browse tables, packages and other objects.");
                ui.add_space(8.0);
                if pythia_ui::icons::icon_text_button(ui, Icon::Connect, "Connect…").clicked() {
                    effects.open.push(OpenRequest::ConnectDialog);
                }
            });
            return effects;
        };

        // Header: connection, schema, refresh, filter.
        ui.horizontal(|ui| {
            if connections.len() > 1 {
                egui::ComboBox::from_id_salt("browser-connection")
                    .selected_text(&connection)
                    .show_ui(ui, |ui| {
                        for c in connections {
                            if ui.selectable_label(*c == connection, c).clicked()
                                && *c != connection
                            {
                                effects.open.push(OpenRequest::Browse {
                                    connection: c.clone(),
                                });
                            }
                        }
                    });
            }
            let schema = self.schema.clone().unwrap_or_default();
            let mut chosen = None;
            egui::ComboBox::from_id_salt("browser-schema")
                .selected_text(if schema.is_empty() {
                    "No database"
                } else {
                    &schema
                })
                .height(400.0)
                .show_ui(ui, |ui| match &self.schemas {
                    Some(Loaded::Ready(list)) => {
                        for s in list {
                            if ui.selectable_label(*s == schema, s).clicked() {
                                chosen = Some(s.clone());
                            }
                        }
                    }
                    Some(Loaded::Failed(e)) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    _ => {
                        ui.spinner();
                    }
                });
            if let Some(s) = chosen {
                self.set_schema(s, &mut effects);
            }
            if pythia_ui::icons::icon_button(ui, Icon::Refresh, "Refresh").clicked() {
                self.refresh(&mut effects);
            }
            let invalid = self.dialect.supports(Feature::InvalidObjects);
            if schema != "PUBLIC"
                && invalid
                && pythia_ui::icons::icon_button(
                    ui,
                    Icon::Compile,
                    "Recompile invalid objects in this schema",
                )
                .clicked()
                && let Some(owner) = self.schema.clone()
            {
                // In the invalid objects window: dependency order, progress.
                effects.open.push(OpenRequest::InvalidObjects {
                    connection: connection.clone(),
                    schema: Some(owner),
                    recompile: true,
                });
            }
        });
        ui.horizontal(|ui| {
            let filtered = self.invalid_only || self.changed != Changed::Any;
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(if filtered {
                        "Filter objects… (more filters on)"
                    } else {
                        "Filter objects…"
                    })
                    .desired_width(ui.available_width() - 34.0),
            );
            let icon = if filtered {
                Icon::Filter.image(ui).tint(ui.visuals().hyperlink_color)
            } else {
                Icon::Filter.image(ui)
            };
            let invalid = self.dialect.supports(Feature::InvalidObjects);
            ui.menu_image_button(icon, |ui| {
                if invalid {
                    ui.checkbox(&mut self.invalid_only, "Invalid objects only");
                    ui.separator();
                }
                ui.label(RichText::new("Changed").strong());
                for (value, label) in [
                    (Changed::Any, "Any time"),
                    (Changed::Day, "Last 24 hours"),
                    (Changed::Week, "Last 7 days"),
                    (Changed::Month, "Last 30 days"),
                ] {
                    ui.radio_value(&mut self.changed, value, label);
                }
            })
            .response
            .on_hover_text("Filter by status and change date");
        });
        ui.add_space(2.0);

        let owner = self.schema.clone().unwrap_or_default();
        if self.last_database().is_some() && matches!(self.schemas, Some(Loaded::Loading)) {
            // The database last used opens once the list is in.
            ui.add_space(12.0);
            ui.vertical_centered(|ui| ui.spinner());
            return effects;
        }
        if owner.is_empty() && self.dialect == Dialect::MYSQL {
            // A connection without a database (none in its address or
            // profile): statements need one too.
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.weak("No database selected");
                ui.add_space(4.0);
                ui.weak("Pick one above: the object browser shows it, and SQL windows run in it.");
            });
            return effects;
        }
        let rows = self.rows(&owner);
        let row_height = pythia_ui::row_height(ui);
        let mut toggles = Vec::new();
        let mut actions = Vec::new();
        let revealed = self.reveal.as_ref().and_then(|target| {
            rows.iter().position(
                |r| matches!(r, Row::Object { object, .. } if object_ref(object) == *target),
            )
        });
        let mut scroll = egui::ScrollArea::both().auto_shrink(false);
        if let Some(row) = revealed {
            let above = (ui.available_height() / row_height / 3.0).floor();
            scroll = scroll.vertical_scroll_offset((row as f32 - above).max(0.0) * row_height);
        }
        scroll.show_rows(ui, row_height, rows.len(), |ui, range| {
            for row in &rows[range] {
                self.row_ui(ui, row, row_height, &mut toggles, &mut actions);
            }
        });
        drop(rows);
        if revealed.is_some() {
            self.reveal = None;
        } else if let Some(target) = self.reveal.clone()
            && let Some(Loaded::Ready(objects)) = self
                .folders
                .get(&(target.owner.clone(), target.object_type))
        {
            // Loaded: either filtered out (show it anyway) or not there.
            if objects.iter().any(|o| object_ref(o) == target) {
                self.filter.clear();
                self.invalid_only = false;
                self.changed = Changed::Any;
            } else {
                self.reveal = None;
                effects
                    .log
                    .push(format!("{} not found", named(self.dialect, &target)));
            }
        }

        for node in toggles {
            if !self.expanded.remove(&node) {
                match &node {
                    Node::Folder(owner, t) if !self.folders.contains_key(&(owner.clone(), *t)) => {
                        self.load_folder(&mut effects, &owner.clone(), *t);
                    }
                    Node::Object(o) if !self.children.contains_key(o) => {
                        self.load_children(&mut effects, &o.clone())
                    }
                    Node::Section(section) => {
                        if let Some(t) = section.accounts()
                            && !self.accounts.contains_key(&t)
                        {
                            self.load_accounts(&mut effects, t);
                        }
                    }
                    _ => {}
                }
                self.expanded.insert(node);
            }
        }
        for action in actions {
            self.action(action, &connection, ui.ctx(), &mut effects);
        }
        self.drop_confirm_ui(ui.ctx(), &mut effects);
        effects
    }

    fn rows<'a>(&'a self, owner: &'a str) -> Vec<Row<'a>> {
        let filter = self.filter.trim().to_uppercase();
        let mut rows = Vec::new();
        let connection = self.connection.as_deref().unwrap_or_default();
        for (section, list) in [
            (Section::Favorites, self.marks.favorites.get(connection)),
            (Section::Recent, self.marks.recent.get(connection)),
        ] {
            let Some(list) = list.filter(|l| !l.is_empty()) else {
                continue;
            };
            let matching: Vec<&ObjectRef> = list
                .iter()
                .filter(|o| name_matches(&o.name, &filter))
                .collect();
            if matching.is_empty() {
                continue;
            }
            let open = self.expanded.contains(&Node::Section(section));
            rows.push(Row::Section {
                section,
                count: Some(matching.len()),
                open,
            });
            if open {
                rows.extend(matching.into_iter().map(Row::Shortcut));
            }
        }
        // Change date cutoff, compared as text with LAST_DDL_TIME.
        let cutoff = self.changed.days().map(|d| {
            (chrono::Local::now() - chrono::Duration::days(d))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        });
        let shown = |o: &DbObject| {
            name_matches(&o.name, &filter)
                && (!self.invalid_only || !o.valid)
                && cutoff.as_deref().is_none_or(|c| o.last_ddl.as_str() >= c)
        };
        let filtering = !filter.is_empty() || self.invalid_only || cutoff.is_some();
        for &object_type in folders(self.dialect, self.mariadb, owner) {
            let node = Node::Folder(owner.to_owned(), object_type);
            let open = self.expanded.contains(&node);
            let folder = self.folders.get(&(owner.to_owned(), object_type));
            let matching: Vec<&DbObject> = match folder {
                Some(Loaded::Ready(objects)) => objects.iter().filter(|o| shown(o)).collect(),
                _ => Vec::new(),
            };
            let count = matches!(folder, Some(Loaded::Ready(_))).then_some(matching.len());
            if filtering && count == Some(0) {
                continue;
            }
            rows.push(Row::Folder {
                owner,
                object_type,
                count,
                open,
            });
            if !open {
                continue;
            }
            match folder {
                Some(Loaded::Loading) | None => rows.push(Row::Message {
                    depth: 1,
                    text: "Loading…".into(),
                    error: false,
                }),
                Some(Loaded::Failed(e)) => rows.push(Row::Message {
                    depth: 1,
                    text: e.clone(),
                    error: true,
                }),
                Some(Loaded::Ready(_)) if matching.is_empty() => rows.push(Row::Message {
                    depth: 1,
                    text: "(none)".into(),
                    error: false,
                }),
                Some(Loaded::Ready(_)) => {}
            }
            for object in matching {
                let r = object_ref(object);
                let expandable = object_type.has_data()
                    || matches!(object_type, ObjectType::Package | ObjectType::Type);
                let open = expandable && self.expanded.contains(&Node::Object(r.clone()));
                rows.push(Row::Object {
                    object,
                    open,
                    expandable,
                });
                if !open {
                    continue;
                }
                match self.children.get(&r) {
                    Some(Loaded::Ready(Children::Columns(cols))) => {
                        let parts = self.parts.get(&r);
                        rows.extend(cols.iter().map(|c| {
                            let fk = parts.is_some_and(|p| p.fk_columns.contains(&c.name));
                            Row::Column(c, fk)
                        }));
                        if let Some(parts) = parts {
                            for (group, items) in [
                                (Group::Indexes, &parts.indexes),
                                (Group::Constraints, &parts.constraints),
                                (Group::Triggers, &parts.triggers),
                                (Group::ReferencedBy, &parts.referenced_by),
                            ] {
                                if items.is_empty() {
                                    continue;
                                }
                                let open = self.expanded.contains(&Node::Group(r.clone(), group));
                                rows.push(Row::Group {
                                    table: r.clone(),
                                    group,
                                    count: items.len(),
                                    open,
                                });
                                if open {
                                    rows.extend(items.iter().map(|p| Row::Part(p, r.clone())));
                                }
                            }
                        }
                    }
                    Some(Loaded::Ready(Children::Members(members))) if members.is_empty() => rows
                        .push(Row::Message {
                            depth: 2,
                            text: "(no subprograms)".into(),
                            error: false,
                        }),
                    Some(Loaded::Ready(Children::Members(members))) => {
                        rows.extend(members.iter().map(|m| Row::Member(m, r.clone())))
                    }
                    Some(Loaded::Failed(e)) => rows.push(Row::Message {
                        depth: 2,
                        text: e.clone(),
                        error: true,
                    }),
                    _ => rows.push(Row::Message {
                        depth: 2,
                        text: "Loading…".into(),
                        error: false,
                    }),
                }
            }
        }
        for section in Section::DATABASE {
            let Some(object_type) = section
                .accounts()
                .filter(|t| self.dialect.account_types().contains(t))
            else {
                continue;
            };
            let open = self.expanded.contains(&Node::Section(section));
            let list = self.accounts.get(&object_type);
            let matching: Vec<&(String, String)> = match list {
                Some(Loaded::Ready(names)) => names
                    .iter()
                    .filter(|(n, _)| name_matches(n, &filter))
                    .collect(),
                _ => Vec::new(),
            };
            let count = matches!(list, Some(Loaded::Ready(_))).then_some(matching.len());
            if filtering && count == Some(0) {
                continue;
            }
            rows.push(Row::Section {
                section,
                count,
                open,
            });
            if !open {
                continue;
            }
            match list {
                Some(Loaded::Loading) | None => rows.push(Row::Message {
                    depth: 1,
                    text: "Loading…".into(),
                    error: false,
                }),
                Some(Loaded::Failed(e)) => rows.push(Row::Message {
                    depth: 1,
                    text: e.clone(),
                    error: true,
                }),
                Some(Loaded::Ready(_)) => rows.extend(
                    matching
                        .into_iter()
                        .map(|(name, detail)| Row::Account(name, detail, object_type)),
                ),
            }
        }
        rows
    }

    fn row_ui(
        &self,
        ui: &mut Ui,
        row: &Row,
        height: f32,
        toggles: &mut Vec<Node>,
        actions: &mut Vec<ObjectAction>,
    ) {
        // A level of the tree: the indent setting and a little more, for
        // the chevron.
        let indent = ui.spacing().indent + 2.0;
        let names = crate::theme::object_names(ui.style());
        let name = |text: String| RichText::new(text).font(names.clone());
        let small = IconSize::Small;
        // Chevron toggle, or the same space when there's nothing to expand.
        let chevron = |ui: &mut Ui, open: bool, expandable: bool| -> bool {
            if !expandable {
                ui.add_space(small.points());
                return false;
            }
            let icon = if open {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            let color = ui.visuals().weak_text_color();
            ui.add(
                icon.mono(color)
                    .fit_to_exact_size(egui::vec2(small.points(), small.points()))
                    .sense(Sense::click()),
            )
            .clicked()
        };
        ui.horizontal(|ui| {
            ui.set_height(height);
            ui.spacing_mut().item_spacing.x = 5.0;
            match row {
                Row::Folder {
                    owner,
                    object_type,
                    count,
                    open,
                } => {
                    let toggle = chevron(ui, *open, true);
                    let folder = if *open {
                        Icon::FolderOpen
                    } else {
                        Icon::Folder
                    };
                    ui.add(folder.sized(ui, small));
                    let label = ui.add(
                        egui::Label::new(RichText::new(object_type.plural()).strong())
                            .sense(Sense::click()),
                    );
                    if let Some(n) = count {
                        ui.weak(n.to_string());
                    }
                    if toggle || label.clicked() {
                        toggles.push(Node::Folder((*owner).to_owned(), *object_type));
                    }
                    let create =
                        can_create(*object_type) && self.dialect.supports(Feature::ObjectEditors);
                    // What the schema's tables need (#145).
                    let scan = *object_type == ObjectType::Table
                        && crate::maintenance::has_tasks(self.dialect, ObjectType::Table);
                    if create || scan {
                        label.context_menu(|ui| {
                            if create {
                                let kind = object_type.dictionary_name().to_lowercase();
                                let label = if *owner == "PUBLIC" {
                                    format!("New public {kind}…")
                                } else {
                                    format!("New {kind}…")
                                };
                                if ui.button(label).clicked() {
                                    actions
                                        .push(ObjectAction::New((*owner).to_owned(), *object_type));
                                    ui.close();
                                }
                            }
                            if scan
                                && ui
                                    .button("Maintenance scan…")
                                    .on_hover_text(
                                        "What the tables need: statistics, unusable or \
                                         fragmented indexes, dead rows, space",
                                    )
                                    .clicked()
                            {
                                actions.push(ObjectAction::MaintenanceScan((*owner).to_owned()));
                                ui.close();
                            }
                        });
                    }
                }
                Row::Object {
                    object,
                    open,
                    expandable,
                } => {
                    ui.add_space(indent);
                    let r = object_ref(object);
                    if chevron(ui, *open, *expandable) {
                        toggles.push(Node::Object(r.clone()));
                    }
                    let icon = Icon::for_object_type(object.object_type.dictionary_name())
                        .unwrap_or(Icon::Text);
                    let icon_response = ui.add(icon.sized(ui, small));
                    if !object.valid {
                        paint_invalid_badge(ui, icon_response.rect);
                    }
                    let mut text = name(shown_name(self.dialect, &object.name));
                    if !object.valid {
                        text = text.color(ui.visuals().error_fg_color);
                    }
                    let selected = self.selected.as_ref() == Some(&r);
                    let response = self.drag_source(ui, &r, |ui| {
                        ui.add(
                            egui::Button::selectable(selected, text).sense(Sense::click_and_drag()),
                        )
                    });
                    let response = response.on_hover_ui(|ui| {
                        ui.label(RichText::new(named(self.dialect, &r)).strong());
                        ui.label(object.object_type.dictionary_name().to_lowercase());
                        if !object.valid {
                            ui.colored_label(ui.visuals().error_fg_color, "Invalid");
                        }
                        ui.weak(format!("Last DDL: {}", object.last_ddl));
                    });
                    if response.clicked() {
                        actions.push(ObjectAction::Select(r.clone()));
                    }
                    if response.double_clicked() {
                        actions.push(ObjectAction::Default(r.clone()));
                    }
                    response.context_menu(|ui| self.context_menu(ui, &r, actions));
                }
                Row::Column(c, fk) => {
                    ui.add_space(indent * 2.0 + small.points());
                    let icon = if c.pk_position.is_some() {
                        Icon::PrimaryKey
                    } else if *fk {
                        Icon::ForeignKey
                    } else {
                        Icon::Column
                    };
                    ui.add(icon.sized(ui, small));
                    let column = written(self.dialect, &c.name);
                    let label = ui.add(
                        egui::Label::new(name(column.clone()))
                            .sense(Sense::drag())
                            .selectable(false),
                    );
                    drag_text(ui, &label, column);
                    let null = if c.nullable { "" } else { " not null" };
                    let label = ui.weak(format!("{}{null}", c.data_type.to_lowercase()));
                    if let Some(comment) = &c.comment {
                        label.on_hover_text(comment);
                    }
                }
                Row::Group {
                    table,
                    group,
                    count,
                    open,
                } => {
                    ui.add_space(indent * 2.0);
                    if chevron(ui, *open, true) {
                        toggles.push(Node::Group(table.clone(), *group));
                    }
                    let label = ui.add(
                        egui::Label::new(RichText::new(group.label()).strong())
                            .sense(Sense::click()),
                    );
                    if label.clicked() {
                        toggles.push(Node::Group(table.clone(), *group));
                    }
                    if *group != Group::ReferencedBy {
                        label.context_menu(|ui| group_menu(ui, table, *group, actions));
                    }
                    ui.weak(count.to_string());
                }
                Row::Part(part, table) => {
                    ui.add_space(indent * 3.0 + small.points());
                    let icon = match part.kind {
                        PartKind::Index { .. } => Icon::Index,
                        PartKind::Constraint { kind: 'P' } => Icon::PrimaryKey,
                        PartKind::Constraint { kind: 'R' } => Icon::ForeignKey,
                        PartKind::Constraint { .. } => Icon::Constraint,
                        PartKind::Trigger => Icon::Trigger,
                    };
                    ui.add(icon.sized(ui, small));
                    let part_name = shown_name(self.dialect, &part.name);
                    let mut text = name(part_name.clone());
                    if !part.enabled {
                        text = text.strikethrough().color(ui.visuals().weak_text_color());
                    }
                    let label = ui.add(egui::Label::new(text).sense(Sense::click()));
                    let tag = match part.kind {
                        PartKind::Index { unique: true } => "unique ",
                        PartKind::Constraint { kind: 'U' } => "unique ",
                        PartKind::Constraint { kind: 'C' } => "check ",
                        _ => "",
                    };
                    ui.weak(format!("{tag}{}", part.detail));
                    let state = if part.enabled { "" } else { " (disabled)" };
                    let label =
                        label.on_hover_text(format!("{part_name}{state}\n{tag}{}", part.detail));
                    let trigger = ObjectRef {
                        owner: table.owner.clone(),
                        name: part.name.clone(),
                        object_type: ObjectType::Trigger,
                    };
                    if part.kind == PartKind::Trigger && label.double_clicked() {
                        actions.push(ObjectAction::Edit(trigger.clone()));
                    }
                    label.context_menu(|ui| {
                        if part.kind == PartKind::Trigger && ui.button("Edit trigger").clicked() {
                            actions.push(ObjectAction::Edit(trigger.clone()));
                            ui.close();
                        }
                        if matches!(part.kind, PartKind::Index { .. })
                            && ui.button("Maintenance…").clicked()
                        {
                            actions.push(ObjectAction::Maintenance(ObjectRef {
                                object_type: ObjectType::Index,
                                ..trigger.clone()
                            }));
                            ui.close();
                        }
                        if ui.button("Copy name").clicked() {
                            ui.ctx().copy_text(part_name.clone());
                            ui.close();
                        }
                    });
                }
                Row::Member(m, package) => {
                    ui.add_space(indent * 2.0 + small.points());
                    let icon = if m.is_function {
                        Icon::Function
                    } else {
                        Icon::Procedure
                    };
                    ui.add(icon.sized(ui, small));
                    let label = ui.add(
                        egui::Label::new(name(shown_name(self.dialect, &m.name)))
                            .sense(Sense::click()),
                    );
                    ui.weak(&m.signature);
                    label.context_menu(|ui| {
                        let subprogram = || Subprogram {
                            name: m.name.clone(),
                            overload: m.overload.as_deref().and_then(|o| o.parse().ok()),
                        };
                        if ui.button("Test…").clicked() {
                            actions.push(ObjectAction::Test(package.clone(), Some(subprogram())));
                            ui.close();
                        }
                        if ui.button("Debug…").clicked() {
                            actions.push(ObjectAction::Debug(package.clone(), Some(subprogram())));
                            ui.close();
                        }
                        if ui.button("Edit package body").clicked() {
                            let body = ObjectRef {
                                object_type: ObjectType::PackageBody,
                                ..package.clone()
                            };
                            actions.push(ObjectAction::Edit(body));
                            ui.close();
                        }
                    });
                }
                Row::Section {
                    section,
                    count,
                    open,
                } => {
                    if chevron(ui, *open, true) {
                        toggles.push(Node::Section(*section));
                    }
                    let (icon, label) = match section {
                        Section::Favorites => (Icon::Star, "Favorites"),
                        Section::Recent => (Icon::History, "Recent"),
                        Section::Users => (Icon::Schema, "Users"),
                        Section::Roles => (Icon::Lock, "Roles"),
                        Section::Profiles => (Icon::Settings, "Profiles"),
                        Section::Directories => (Icon::Folder, "Directories"),
                    };
                    ui.add(icon.sized(ui, small));
                    let r = ui
                        .add(egui::Label::new(RichText::new(label).strong()).sense(Sense::click()));
                    if r.clicked() {
                        toggles.push(Node::Section(*section));
                    }
                    if let Some(t) = section.accounts().filter(|t| has_form(self.dialect, *t)) {
                        r.context_menu(|ui| {
                            let kind = t.dictionary_name().to_lowercase();
                            if ui.button(format!("New {kind}…")).clicked() {
                                actions.push(ObjectAction::New(String::new(), t));
                                ui.close();
                            }
                        });
                    }
                    if let Some(n) = count {
                        ui.weak(n.to_string());
                    }
                }
                Row::Account(name, detail, object_type) => {
                    ui.add_space(indent);
                    ui.add_space(small.points());
                    let icon =
                        Icon::for_object_type(object_type.dictionary_name()).unwrap_or(Icon::Text);
                    ui.add(icon.sized(ui, small));
                    let account = ObjectRef {
                        owner: String::new(),
                        name: (*name).to_owned(),
                        object_type: *object_type,
                    };
                    let selected = self.selected.as_ref() == Some(&account);
                    let response = ui.add(egui::Button::selectable(
                        selected,
                        RichText::new(shown_name(self.dialect, name)).font(names.clone()),
                    ));
                    let editors = has_form(self.dialect, *object_type);
                    if response.clicked() {
                        actions.push(ObjectAction::Select(account.clone()));
                    }
                    if response.double_clicked() && editors {
                        actions.push(ObjectAction::EditObject(account.clone()));
                    }
                    let menu = AccountMenu {
                        editors,
                        show_schema: self.has_schema_of(&account),
                    };
                    response.context_menu(|ui| account_menu(ui, &account, menu, actions));
                    if !detail.is_empty() {
                        ui.add(
                            egui::Label::new(RichText::new(*detail).weak().font(names.clone()))
                                .truncate(),
                        );
                    }
                }
                Row::Shortcut(object) => {
                    ui.add_space(indent);
                    ui.add_space(small.points());
                    let icon = Icon::for_object_type(object.object_type.dictionary_name())
                        .unwrap_or(Icon::Text);
                    ui.add(icon.sized(ui, small));
                    let selected = self.selected.as_ref() == Some(*object);
                    let text = if Some(object.owner.as_str()) == self.schema.as_deref() {
                        name(shown_name(self.dialect, &object.name))
                    } else {
                        name(shown_qualified(self.dialect, object))
                    };
                    let response = self.drag_source(ui, object, |ui| {
                        ui.add(
                            egui::Button::selectable(selected, text).sense(Sense::click_and_drag()),
                        )
                    });
                    let response = response.on_hover_text(format!(
                        "{} {}",
                        object.object_type.dictionary_name().to_lowercase(),
                        named(self.dialect, object)
                    ));
                    if response.clicked() {
                        actions.push(ObjectAction::Select((*object).clone()));
                    }
                    if response.double_clicked() {
                        actions.push(ObjectAction::Default((*object).clone()));
                    }
                    response.context_menu(|ui| self.context_menu(ui, object, actions));
                }
                Row::Message { depth, text, error } => {
                    ui.add_space(indent * *depth as f32 + small.points());
                    if *error {
                        ui.colored_label(ui.visuals().error_fg_color, text);
                    } else {
                        ui.weak(text);
                    }
                }
            }
        });
    }

    /// Lets an object's row be dragged into an editor, which inserts its
    /// name (qualified when it's in another schema). `add` must sense drags.
    fn drag_source(
        &self,
        ui: &mut Ui,
        object: &ObjectRef,
        add: impl FnOnce(&mut Ui) -> egui::Response,
    ) -> egui::Response {
        let dialect = self.dialect;
        let name = if Some(object.owner.as_str()) == self.schema.as_deref() {
            written(dialect, &object.name)
        } else {
            written_qualified(dialect, &object.owner, &object.name)
        };
        let response = add(ui);
        drag_text(ui, &response, name);
        response
    }

    fn context_menu(&self, ui: &mut Ui, object: &ObjectRef, actions: &mut Vec<ObjectAction>) {
        let connection = self.connection.as_deref().unwrap_or_default();
        let options = MenuOptions {
            dialect: self.dialect,
            favorite: self.marks.is_favorite(connection, object),
            drop: true,
            reveal: false,
            member: None,
            create: true,
        };
        actions.extend(object_menu(ui, object, &options));
    }

    /// Carries out an action chosen outside the browser, e.g. in an editor's
    /// context menu, on the attached connection.
    pub fn perform(&mut self, action: ObjectAction, ctx: &egui::Context) -> Effects {
        let mut effects = Effects::default();
        if let Some(connection) = self.connection.clone() {
            self.action(action, &connection, ctx, &mut effects);
        }
        effects
    }

    fn action(
        &mut self,
        action: ObjectAction,
        connection: &str,
        ctx: &egui::Context,
        effects: &mut Effects,
    ) {
        let connection = connection.to_owned();
        let dialect = self.dialect;
        let programs = dialect.supports(Feature::PlSql);
        match action {
            ObjectAction::Select(o) => self.selected = Some(o),
            ObjectAction::Default(o) if o.object_type.has_source() && programs => {
                effects.open.push(OpenRequest::Program {
                    connection,
                    object: o,
                });
            }
            ObjectAction::Default(o) | ObjectAction::Describe(o) => {
                effects.open.push(OpenRequest::Object {
                    connection,
                    object: o,
                })
            }
            ObjectAction::Query(o) => effects.open.push(OpenRequest::Sql {
                connection,
                text: format!("select * from {}", dialect.qualified(&o.owner, &o.name)),
                run: true,
            }),
            ObjectAction::Edit(o) if programs => effects.open.push(OpenRequest::Program {
                connection,
                object: o,
            }),
            // Without program windows the DDL becomes a script that
            // replaces the object.
            ObjectAction::Edit(o) => {
                let query = MetaQuery::Ddl {
                    owner: o.owner.clone(),
                    name: o.name.clone(),
                    object_type: o.object_type,
                };
                self.meta(effects, Pending::Script(o), query);
            }
            ObjectAction::Compile(o) => {
                // As the invalid objects window does: keeping each unit's
                // compiler settings, debug information included.
                let id = pythia_db::recompile::ObjectId::new(
                    &o.owner,
                    &o.name,
                    o.object_type.dictionary_name(),
                );
                match pythia_db::recompile::compile_sql(&id) {
                    Some(sql) => self.run(effects, &o, "compiled", sql),
                    None => effects
                        .log
                        .push(format!("{} can't be compiled", o.qualified())),
                }
            }
            ObjectAction::CopyName(o) => ctx.copy_text(o.name.clone()),
            ObjectAction::CopyQualified(o) => ctx.copy_text(dialect.qualified(&o.owner, &o.name)),
            ObjectAction::Drop(o) => self.confirm_drop = Some(o),
            ObjectAction::ToggleFavorite(o) => {
                let opening = !self.marks.is_favorite(&connection, &o);
                self.marks.toggle_favorite(&connection, o);
                if opening {
                    self.expanded.insert(Node::Section(Section::Favorites));
                }
            }
            ObjectAction::EditTable(object, tab) => effects.open.push(OpenRequest::EditTable {
                connection,
                object,
                tab,
            }),
            ObjectAction::New(owner, ObjectType::Table) => {
                effects.open.push(OpenRequest::NewTable {
                    connection,
                    owner,
                    like: None,
                })
            }
            ObjectAction::New(owner, object_type) => effects.open.push(OpenRequest::EditObject {
                connection,
                target: EditTarget::New {
                    owner,
                    object_type,
                    from: None,
                },
            }),
            ObjectAction::NewFrom(ObjectType::Table, like) => {
                effects.open.push(OpenRequest::NewTable {
                    connection,
                    owner: like.owner.clone(),
                    like: Some(like),
                })
            }
            ObjectAction::NewFrom(object_type, from) => {
                // A synonym goes to the schema shown, other objects to the
                // one of the object they're on.
                let owner = match object_type {
                    ObjectType::Synonym => {
                        self.schema.clone().unwrap_or_else(|| from.owner.clone())
                    }
                    _ => from.owner.clone(),
                };
                effects.open.push(OpenRequest::EditObject {
                    connection,
                    target: EditTarget::New {
                        owner,
                        object_type,
                        from: Some(from),
                    },
                })
            }
            ObjectAction::ShowSchema(user) => {
                self.set_schema(user, effects);
            }
            ObjectAction::EditObject(object) => effects.open.push(OpenRequest::EditObject {
                connection,
                target: EditTarget::Edit(object),
            }),
            ObjectAction::Grants(object) => effects.open.push(OpenRequest::EditObject {
                connection,
                target: EditTarget::Grants(object),
            }),
            ObjectAction::Generate(object, kind) => {
                let id = request_id();
                let query = MetaQuery::Columns {
                    owner: object.owner.clone(),
                    table: object.name.clone(),
                    keys: object.object_type == ObjectType::Table,
                };
                self.pending.insert(id, Pending::Generate { object, kind });
                effects.request(Target::Main, DbRequest::Meta { id, query });
            }
            ObjectAction::EditAt(object, member) => effects.open.push(OpenRequest::ProgramMember {
                connection,
                object,
                member,
                parameter: None,
            }),
            ObjectAction::Reveal(object) => self.reveal(object, effects),
            ObjectAction::TestLink(object) => {
                let id = request_id();
                let query = MetaQuery::TestLink {
                    owner: object.owner.clone(),
                    name: object.name.clone(),
                };
                self.pending.insert(id, Pending::TestLink(object));
                effects.request(Target::Meta, DbRequest::Meta { id, query });
            }
            ObjectAction::Maintenance(object) => effects.open.push(OpenRequest::Maintenance {
                connection,
                object,
                task: None,
            }),
            ObjectAction::MaintenanceScan(schema) => {
                // PostgreSQL and SQL Server scan the database.
                let schema = match self.dialect.kind() {
                    pythia_db::DbKind::Oracle | pythia_db::DbKind::MySql => schema,
                    _ => String::new(),
                };
                effects
                    .open
                    .push(OpenRequest::MaintenanceScan { connection, schema });
            }
            ObjectAction::Test(object, member) => self.request_test(object, member, false, effects),
            ObjectAction::Debug(object, member) => self.request_test(object, member, true, effects),
        }
    }

    /// Builds the test block of `member` (or the standalone unit) from its
    /// arguments, to open or to debug.
    fn request_test(
        &mut self,
        object: ObjectRef,
        member: Option<Subprogram>,
        debug: bool,
        effects: &mut Effects,
    ) {
        let standalone = matches!(
            object.object_type,
            ObjectType::Procedure | ObjectType::Function
        );
        let (package, name, overload, qualified) = match member.filter(|_| !standalone) {
            Some(m) => (
                Some(object.name.clone()),
                m.name.clone(),
                m.overload,
                format!(
                    "{}.{}",
                    object.qualified(),
                    crate::documents::quote_ident(&m.name)
                ),
            ),
            None => (None, object.name.clone(), None, object.qualified()),
        };
        let id = request_id();
        let task = crate::test_script::task(
            object.owner.clone(),
            package,
            name,
            overload,
            crate::test_script::lowercase_unquoted(&qualified),
        );
        self.pending.insert(id, Pending::Test { object, debug });
        effects.request(Target::Main, DbRequest::Task { id, task });
    }

    fn drop_confirm_ui(&mut self, ctx: &egui::Context, effects: &mut Effects) {
        let Some(object) = self.confirm_drop.clone() else {
            return;
        };
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new("drop-object")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading("Drop object");
            ui.add_space(6.0);
            ui.label(format!(
                "Drop {} {}? This cannot be undone.",
                object.object_type.dictionary_name().to_lowercase(),
                named(self.dialect, &object)
            ));
            match (self.dialect.kind(), object.object_type) {
                (pythia_db::DbKind::Oracle, ObjectType::User) => {
                    ui.weak("A user who owns objects isn't dropped (that needs CASCADE).");
                }
                (pythia_db::DbKind::Postgres, ObjectType::Function | ObjectType::Procedure) => {
                    ui.weak("Every version of it, whatever its arguments, is dropped.");
                }
                (pythia_db::DbKind::Postgres, ObjectType::User | ObjectType::Role) => {
                    ui.weak(
                        "A role that owns objects or holds privileges isn't dropped (that \
                         needs REASSIGN OWNED and DROP OWNED in each database).",
                    );
                }
                (pythia_db::DbKind::SqlServer, ObjectType::User | ObjectType::Role) => {
                    ui.weak(format!(
                        "A {} that owns a schema isn't dropped.",
                        object.object_type.dictionary_name().to_lowercase()
                    ));
                }
                (_, ObjectType::Profile) => {
                    ui.weak("Users with this profile get the DEFAULT profile.");
                }
                (_, ObjectType::Directory) => {
                    ui.weak("The folder on the server stays; the grants on the directory go.");
                }
                _ => {}
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let drop = egui::Button::new(RichText::new("Drop").color(Color32::WHITE))
                    .fill(Color32::from_rgb(0xc0, 0x30, 0x30));
                if ui.add(drop).clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        if let Some(yes) = decision {
            self.confirm_drop = None;
            if yes {
                self.run(effects, &object, "dropped", drop_sql(self.dialect, &object));
            }
        }
    }
}

/// The New submenu of an object: another object of its type, and objects
/// on it (label, action).
fn new_entries(object: &ObjectRef) -> Vec<(String, ObjectAction)> {
    let t = object.object_type;
    let on = |kind: ObjectType| ObjectAction::NewFrom(kind, object.clone());
    let mut out = Vec::new();
    if can_create(t) {
        let kind = t.dictionary_name().to_lowercase();
        let mut label = format!("{kind}…");
        label[..1].make_ascii_uppercase();
        out.push((label, ObjectAction::New(object.owner.clone(), t)));
    }
    match t {
        ObjectType::Table => out.extend([
            ("Table like this…".to_owned(), on(ObjectType::Table)),
            ("Index on this table…".to_owned(), on(ObjectType::Index)),
            ("Trigger on this table…".to_owned(), on(ObjectType::Trigger)),
            ("View on this table…".to_owned(), on(ObjectType::View)),
        ]),
        ObjectType::View => out.extend([
            ("Trigger on this view…".to_owned(), on(ObjectType::Trigger)),
            ("View on this view…".to_owned(), on(ObjectType::View)),
        ]),
        _ => {}
    }
    if matches!(
        t,
        ObjectType::Table
            | ObjectType::View
            | ObjectType::MaterializedView
            | ObjectType::Sequence
            | ObjectType::Package
            | ObjectType::Procedure
            | ObjectType::Function
            | ObjectType::Type
    ) {
        out.push(("Synonym for this…".to_owned(), on(ObjectType::Synonym)));
    }
    out
}

/// The context menu of a table's Indexes, Constraints or Triggers.
fn group_menu(ui: &mut Ui, table: &ObjectRef, group: Group, actions: &mut Vec<ObjectAction>) {
    let (label, action) = match group {
        Group::Indexes => (
            "New index…",
            ObjectAction::NewFrom(ObjectType::Index, table.clone()),
        ),
        Group::Constraints => (
            "Add constraint…",
            ObjectAction::EditTable(table.clone(), TableTab::Keys),
        ),
        Group::Triggers => (
            "New trigger…",
            ObjectAction::NewFrom(ObjectType::Trigger, table.clone()),
        ),
        Group::ReferencedBy => return,
    };
    if ui.button(label).clicked() {
        actions.push(action);
        ui.close();
    }
}

/// What the context menu of a user, role, profile or directory offers
/// besides copying and dropping it.
#[derive(Clone, Copy)]
struct AccountMenu {
    /// Editing and creating them, and (Oracle's directories) their grants.
    editors: bool,
    /// Showing the schema of the same name a user owns.
    show_schema: bool,
}

/// The context menu of a user, role, profile or directory.
fn account_menu(
    ui: &mut Ui,
    account: &ObjectRef,
    menu: AccountMenu,
    actions: &mut Vec<ObjectAction>,
) {
    let kind = account.object_type.dictionary_name().to_lowercase();
    let mut item = |ui: &mut Ui, label: &str, action: ObjectAction| {
        if ui.button(label).clicked() {
            actions.push(action);
            ui.close();
        }
    };
    if menu.editors {
        item(
            ui,
            &format!("Edit {kind}…"),
            ObjectAction::EditObject(account.clone()),
        );
    }
    if menu.show_schema {
        item(
            ui,
            "Show schema",
            ObjectAction::ShowSchema(account.name.clone()),
        );
    }
    if menu.editors {
        if has_grants(account.object_type) {
            item(ui, "Grants…", ObjectAction::Grants(account.clone()));
        }
        item(
            ui,
            &format!("New {kind}…"),
            ObjectAction::New(String::new(), account.object_type),
        );
        ui.separator();
    } else if menu.show_schema {
        ui.separator();
    }
    item(ui, "Copy name", ObjectAction::CopyName(account.clone()));
    // The DEFAULT profile can't be dropped.
    if !(account.object_type == ObjectType::Profile && account.name == "DEFAULT") {
        ui.separator();
        item(ui, "Drop…", ObjectAction::Drop(account.clone()));
    }
}

/// `DROP` for an object. Public synonyms and links have their own syntax,
/// a link's name can't be qualified (its dots belong to the name), and a
/// scheduler job is dropped through DBMS_SCHEDULER.
fn drop_sql(dialect: Dialect, o: &ObjectRef) -> String {
    let kind = o.object_type.dictionary_name().to_lowercase();
    if dialect == Dialect::POSTGRES && o.object_type == ObjectType::Trigger {
        // PostgreSQL's triggers are named per table.
        return format!(
            "do $$\ndeclare\n  t text;\nbegin\n  \
             for t in select c.oid::regclass from pg_trigger g join pg_class c on c.oid = g.tgrelid \
             join pg_namespace n on n.oid = c.relnamespace \
             where n.nspname = {} and g.tgname = {} loop\n    \
             execute format('drop trigger %I on %s', {}, t);\n  end loop;\nend\n$$",
            pg_literal(&o.owner),
            pg_literal(&o.name),
            pg_literal(&o.name)
        );
    }
    if dialect == Dialect::POSTGRES
        && matches!(o.object_type, ObjectType::Function | ObjectType::Procedure)
    {
        // The entry stands for every argument list of the name, which
        // `drop function name` refuses when there are several (#195).
        let prokind = if o.object_type == ObjectType::Procedure {
            "p"
        } else {
            "f"
        };
        return format!(
            "do $$\ndeclare\n  r regprocedure;\nbegin\n  \
             for r in select p.oid::regprocedure from pg_proc p \
             join pg_namespace n on n.oid = p.pronamespace \
             where n.nspname = {} and p.proname = {} and p.prokind = '{prokind}' loop\n    \
             execute format('drop {kind} %s', r);\n  end loop;\nend\n$$",
            pg_literal(&o.owner),
            pg_literal(&o.name),
        );
    }
    if dialect == Dialect::MYSQL && o.object_type.is_database_object() {
        return format!("drop {kind} {}", mysql_account(&o.name));
    }
    if dialect != Dialect::ORACLE && o.object_type.is_database_object() {
        return format!("drop {kind} {}", dialect.quote_ident(&o.name));
    }
    if dialect != Dialect::ORACLE {
        return format!("drop {kind} {}", dialect.qualified(&o.owner, &o.name));
    }
    match (o.owner.as_str(), o.object_type) {
        (_, ObjectType::Job) => crate::jobs_window::action_sql("drop", &o.owner, &o.name),
        ("PUBLIC", _) => format!("drop public {kind} {}", quote_ident(&o.name)),
        (_, ObjectType::DatabaseLink) => format!("drop {kind} {}", quote_ident(&o.name)),
        // Users with the profile get DEFAULT.
        (_, ObjectType::Profile) => format!("drop {kind} {} cascade", o.qualified()),
        _ => format!("drop {kind} {}", o.qualified()),
    }
}

/// A script that replaces a stored object with its (edited) DDL. MariaDB
/// has `CREATE OR REPLACE`, MySQL for views only: a new version that fails
/// leaves the object as it was, a routine with its grants (#240).
/// MySQL's routines and triggers are dropped, then created, as the script
/// says. A routine or trigger ends with `/` like a PL/SQL unit so the
/// body's `;` stay in it. PostgreSQL's routines and views come with
/// `CREATE OR REPLACE`, its triggers get it; SQL Server's get `CREATE OR
/// ALTER`. SQLite has neither, nor has PostgreSQL for a materialized view,
/// but their DDL is transactional: the drop and the create go in one
/// transaction that a failing create undoes (#286). SQLite's script begins
/// one (which fails before anything is changed when the window has one
/// pending) and rolls it back on an error; PostgreSQL's runs both in one
/// `DO` block. A SQLite trigger's body keeps its `;` without a `/`, and the
/// `;` after a line comment goes on a line of its own (#206).
///
/// The object is created where it was, whatever the current database or
/// schema (#248): MySQL's DDL leaves out the database, so the script
/// starts with `use`; SQL Server's and SQLite's get the schema in front of
/// the name they create.
pub fn replace_script(dialect: Dialect, o: &ObjectRef, ddl: &str, mariadb: bool) -> String {
    let ddl = ddl.trim_end();
    let qualified = dialect.qualified(&o.owner, &o.name);
    let kind = o.object_type.dictionary_name().to_lowercase();
    let database = format!("use {};\n\n", dialect.quote_ident(&o.owner));
    match (dialect.kind(), o.object_type) {
        (pythia_db::DbKind::MySql, ObjectType::View) => {
            format!("{database}{}\n", create_or(ddl, "or replace", ""))
        }
        (pythia_db::DbKind::MySql, _) if mariadb => {
            format!("{database}{}\n/\n", create_or(ddl, "or replace", ""))
        }
        (pythia_db::DbKind::MySql, _) => {
            let warning = match o.object_type {
                ObjectType::Procedure | ObjectType::Function => format!(
                    "-- MySQL can't replace a {kind} in place: the drop revokes the grants\n\
                     -- on it, and if the create fails, the {kind} is gone.\n"
                ),
                _ => format!(
                    "-- MySQL can't replace a {kind} in place: if the create fails, the\n\
                     -- {kind} is gone.\n"
                ),
            };
            format!("{database}{warning}drop {kind} if exists {qualified};\n\n{ddl}\n/\n")
        }
        (pythia_db::DbKind::Postgres, ObjectType::Trigger) => {
            format!("{}\n", create_or(ddl, "or replace", "TRIGGER"))
        }
        (pythia_db::DbKind::Postgres, ObjectType::MaterializedView) => {
            let ddl = pythia_plsql::terminated(ddl, pythia_plsql::Syntax::Postgres);
            let mut tag = "$pythia$".to_owned();
            while ddl.contains(&tag) {
                tag.insert(tag.len() - 1, '_');
            }
            // A column named `found` is the view's, not PL/pgSQL's FOUND.
            format!(
                "-- One block: if the create fails, the {kind} stays as it was.\n\
                 do {tag}\n#variable_conflict use_column\nbegin\n\
                 drop {kind} if exists {qualified};\n\n{ddl}\nend\n{tag};\n"
            )
        }
        (pythia_db::DbKind::Postgres, _) => format!("{ddl}\n"),
        (pythia_db::DbKind::SqlServer, _) => {
            let ddl = in_schema(dialect, ddl, &o.owner);
            format!("{}\n", create_or(&ddl, "or alter", ""))
        }
        (pythia_db::DbKind::Sqlite, _) => {
            let ddl = if o.owner == "main" {
                ddl.to_owned()
            } else {
                in_schema(dialect, ddl, &o.owner)
            };
            format!(
                "-- A transaction of its own: if the create fails, the {kind} stays\n\
                 -- as it was.\n\
                 begin;\nwhenever sqlerror exit rollback\n\n\
                 drop {kind} if exists {qualified};\n\n{}\n\ncommit;\n",
                pythia_plsql::terminated(&ddl, pythia_plsql::Syntax::Sqlite)
            )
        }
        _ => format!("drop {kind} if exists {qualified};\n\n{ddl}\n/\n"),
    }
}

/// `ddl` with `schema` in front of the name it creates, unless it names
/// one (#248): SQL Server creates an unqualified name in the session's
/// default schema, SQLite in `main`. A SQL Server trigger's table gets it
/// too, as it must be in the trigger's schema; SQLite's takes the
/// trigger's.
fn in_schema(dialect: Dialect, ddl: &str, schema: &str) -> String {
    const MODIFIERS: &[&str] = &["OR", "ALTER", "REPLACE", "TEMP", "TEMPORARY", "UNIQUE"];
    const KINDS: &[&str] = &[
        "VIEW",
        "TRIGGER",
        "INDEX",
        "TABLE",
        "PROC",
        "PROCEDURE",
        "FUNCTION",
    ];
    let tokens = pythia_plsql::tokenize_in(ddl, dialect.syntax());
    let words: Vec<_> = tokens.iter().filter(|t| !t.kind.is_trivia()).collect();
    let text = |t: &pythia_plsql::Token| &ddl[t.span.clone()];
    let is = |i: usize, any: &[&str]| {
        words.get(i).is_some_and(|t| {
            t.kind.is_word() && any.iter().any(|w| text(t).eq_ignore_ascii_case(w))
        })
    };
    // Where a name that doesn't name its schema starts.
    let unqualified = |i: usize| {
        let dot = |t: &&pythia_plsql::Token| t.kind == pythia_plsql::TokenKind::Dot;
        let qualified = words.get(i + 1).is_some_and(dot);
        words.get(i).filter(|_| !qualified).map(|t| t.span.start)
    };
    if !is(0, &["CREATE"]) {
        return ddl.to_owned();
    }
    let mut i = 1;
    while is(i, MODIFIERS) {
        i += 1;
    }
    let trigger = is(i, &["TRIGGER"]);
    if !is(i, KINDS) {
        return ddl.to_owned();
    }
    i += 1;
    if is(i, &["IF"]) && is(i + 1, &["NOT"]) && is(i + 2, &["EXISTS"]) {
        i += 3;
    }
    let mut at: Vec<usize> = unqualified(i).into_iter().collect();
    if trigger && dialect.kind() == pythia_db::DbKind::SqlServer {
        // Not a database's or the server's trigger (`ON DATABASE`).
        if let Some(on) = (i + 1..words.len()).find(|&j| is(j, &["ON"]))
            && !is(on + 1, &["DATABASE", "ALL"])
        {
            at.extend(unqualified(on + 1));
        }
    }
    let mut out = ddl.to_owned();
    for start in at.into_iter().rev() {
        out.insert_str(start, &format!("{}.", dialect.quote_ident(schema)));
    }
    out
}

/// `ddl` with `words` after its leading `CREATE` (when followed by `next`,
/// if given), unless it has them already.
fn create_or(ddl: &str, words: &str, next: &str) -> String {
    let tokens = pythia_plsql::tokenize(ddl);
    let mut significant = tokens.iter().filter(|t| !t.kind.is_trivia());
    match (significant.next(), significant.next()) {
        (Some(create), Some(after))
            if ddl[create.span.clone()].eq_ignore_ascii_case("create")
                && !ddl[after.span.clone()].eq_ignore_ascii_case("or")
                && (next.is_empty() || ddl[after.span.clone()].eq_ignore_ascii_case(next)) =>
        {
            format!(
                "{} {words}{}",
                &ddl[..create.span.end],
                &ddl[create.span.end..]
            )
        }
        _ => ddl.to_owned(),
    }
}

/// A PostgreSQL string literal.
fn pg_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The object as its database's SQL names it, as its `DROP` does: quoted
/// only where it needs it, a MySQL account as `'user'@'host'` (#268).
fn named(dialect: Dialect, o: &ObjectRef) -> String {
    if dialect == Dialect::MYSQL && o.object_type.is_database_object() {
        return mysql_account(&o.name);
    }
    o.qualified_in(dialect)
}

/// A MySQL account as listed (`user@host`, a role without its host) as
/// `'user'@'host'`; the host is the last `@`'s, user names may have one.
fn mysql_account(name: &str) -> String {
    let literal = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"));
    match name.rsplit_once('@') {
        Some((user, host)) => format!("{}@{}", literal(user), literal(host)),
        None => literal(name),
    }
}

/// Entries of an object's context menu besides those for its type.
pub struct MenuOptions {
    /// The connection's database: what it offers for the object.
    pub dialect: Dialect,
    /// The object is a favorite: the entry removes it.
    pub favorite: bool,
    /// Offer Drop… (left out in editors, where it's too easy to hit).
    pub drop: bool,
    /// Offer "Show in object browser".
    pub reveal: bool,
    /// A subprogram of the package, e.g. `PROC` in `pkg.proc`.
    pub member: Option<String>,
    /// Offer the New submenu (new objects of the type, or on this one).
    pub create: bool,
}

/// The objects *Edit* opens: with program windows (PL/SQL) its stored
/// units; elsewhere a script that replaces the object with its DDL
/// ([`replace_script`]), for routines and triggers, and for views,
/// PostgreSQL's materialized views and SQLite's indexes, whose scripts
/// leave the object as it was when the new version fails (#240, #286).
/// The menus offered it for routines and triggers only (#300).
pub fn has_edit(dialect: Dialect, t: ObjectType) -> bool {
    if t.has_source() {
        return true;
    }
    if dialect.supports(Feature::PlSql) {
        return false;
    }
    match t {
        ObjectType::View => true,
        ObjectType::MaterializedView => dialect == Dialect::POSTGRES,
        ObjectType::Index => dialect == Dialect::SQLITE,
        _ => false,
    }
}

/// The context menu of a database object, shared by the object browser and
/// the editors. Returns the chosen action.
pub fn object_menu(ui: &mut Ui, object: &ObjectRef, options: &MenuOptions) -> Option<ObjectAction> {
    let t = object.object_type;
    let plsql = options.dialect.supports(Feature::PlSql);
    let editors = options.dialect.supports(Feature::ObjectEditors);
    let mut chosen = None;
    let mut item = |ui: &mut Ui, label: &str, action: ObjectAction| {
        if ui.button(label).clicked() {
            chosen = Some(action);
            ui.close();
        }
    };
    if t.has_data() {
        item(ui, "Query data", ObjectAction::Query(object.clone()));
    }
    if has_edit(options.dialect, t) {
        match &options.member {
            Some(member) => item(
                ui,
                &format!("Edit {}", shown_name(options.dialect, member)),
                ObjectAction::EditAt(object.clone(), member.clone()),
            ),
            None => item(ui, "Edit", ObjectAction::Edit(object.clone())),
        }
    }
    let testable = matches!(t, ObjectType::Procedure | ObjectType::Function)
        || (t == ObjectType::Package && options.member.is_some());
    if testable && plsql {
        // A reference doesn't say which overload: the first.
        let member = || {
            options.member.clone().map(|name| Subprogram {
                name,
                overload: None,
            })
        };
        item(ui, "Test…", ObjectAction::Test(object.clone(), member()));
        if options.dialect.supports(Feature::Debugger) {
            item(ui, "Debug…", ObjectAction::Debug(object.clone(), member()));
        }
    }
    item(ui, "Describe", ObjectAction::Describe(object.clone()));
    if t == ObjectType::Table && editors {
        item(
            ui,
            "Edit table…",
            ObjectAction::EditTable(object.clone(), TableTab::Columns),
        );
    }
    if has_editor(t) && editors {
        item(ui, "Edit…", ObjectAction::EditObject(object.clone()));
    }
    if has_grants(t) && editors {
        item(ui, "Grants…", ObjectAction::Grants(object.clone()));
    }
    if options.create && editors {
        let news = new_entries(object);
        if !news.is_empty() {
            ui.menu_button("New", |ui| {
                for (label, action) in news {
                    item(ui, &label, action);
                }
            });
        }
    }
    if t.has_data() {
        ui.menu_button("Generate", |ui| {
            for kind in [
                Template::Select,
                Template::Insert,
                Template::Update,
                Template::ColumnList,
            ] {
                item(
                    ui,
                    kind.label(),
                    ObjectAction::Generate(object.clone(), kind),
                );
            }
        });
    }
    ui.separator();
    if plsql && (t.has_source() || matches!(t, ObjectType::View | ObjectType::MaterializedView)) {
        item(ui, "Compile", ObjectAction::Compile(object.clone()));
    }
    if t == ObjectType::DatabaseLink {
        item(ui, "Test link", ObjectAction::TestLink(object.clone()));
    }
    if crate::maintenance::has_tasks(options.dialect, t) {
        item(
            ui,
            "Maintenance…",
            ObjectAction::Maintenance(object.clone()),
        );
    }
    let favorite = if options.favorite {
        "Remove from favorites"
    } else {
        "Add to favorites"
    };
    item(ui, favorite, ObjectAction::ToggleFavorite(object.clone()));
    if options.reveal {
        item(
            ui,
            "Show in object browser",
            ObjectAction::Reveal(object.clone()),
        );
    }
    item(ui, "Copy name", ObjectAction::CopyName(object.clone()));
    item(
        ui,
        "Copy qualified name",
        ObjectAction::CopyQualified(object.clone()),
    );
    if options.drop {
        ui.separator();
        item(ui, "Drop…", ObjectAction::Drop(object.clone()));
    }
    chosen
}

/// A package's subprogram to test or debug: its name and, if it is
/// overloaded and known, which overload (`ALL_PROCEDURES.OVERLOAD`, which
/// numbers them in the order of the spec).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subprogram {
    pub name: String,
    pub overload: Option<u32>,
}

/// What can be done with a database object, from the browser or an editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectAction {
    Select(ObjectRef),
    Default(ObjectRef),
    Query(ObjectRef),
    Edit(ObjectRef),
    Describe(ObjectRef),
    Compile(ObjectRef),
    CopyName(ObjectRef),
    CopyQualified(ObjectRef),
    Drop(ObjectRef),
    Test(ObjectRef, Option<Subprogram>),
    /// Test… in a SQL window that debugs right away, stopping on entry to
    /// the subprogram.
    Debug(ObjectRef, Option<Subprogram>),
    /// Open the package at this subprogram.
    EditAt(ObjectRef, String),
    /// Show the object in the browser's tree.
    Reveal(ObjectRef),
    Generate(ObjectRef, Template),
    EditTable(ObjectRef, TableTab),
    /// Form editor (or table builder) for a new object of this type in this
    /// schema.
    New(String, ObjectType),
    /// A new object of this type on (or like) an object: an index on a
    /// table, a synonym for it, a table like it.
    NewFrom(ObjectType, ObjectRef),
    /// Browse a user's schema.
    ShowSchema(String),
    EditObject(ObjectRef),
    Grants(ObjectRef),
    ToggleFavorite(ObjectRef),
    /// Query over a database link and report whether it works.
    TestLink(ObjectRef),
    /// Statistics, rebuilds, vacuum and the like of a table or index.
    Maintenance(ObjectRef),
    /// What a schema's tables need (#145).
    MaintenanceScan(String),
}

/// Statements generated from a table's columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    Select,
    Insert,
    Update,
    ColumnList,
}

impl Template {
    fn label(self) -> &'static str {
        match self {
            Template::Select => "SELECT statement",
            Template::Insert => "INSERT statement",
            Template::Update => "UPDATE statement",
            Template::ColumnList => "Column list",
        }
    }
}

/// `name` as the browser, tabs and titles show it (#272): as it is, so
/// that names told apart by case read apart. Oracle's names in its default
/// uppercase read in lowercase, which reads better, and the others as its
/// SQL writes them (`"Foo"`), so that they don't read like those. A name
/// that is empty or only spaces reads quoted too, and so does one with
/// spaces around it.
pub fn shown_name(dialect: Dialect, name: &str) -> String {
    if quoted_to_show(name) {
        // There would be nothing to see or click (#281), or spaces that
        // don't show: `" t"` read like `t` (#298).
        dialect.quote_ident(name)
    } else if dialect != Dialect::ORACLE {
        name.to_owned()
    } else if name.chars().any(char::is_lowercase) {
        dialect.quote_ident(name)
    } else {
        name.to_lowercase()
    }
}

/// The name reads quoted wherever it is shown: it is empty, or has white
/// space at its start or end, which wouldn't show.
pub fn quoted_to_show(name: &str) -> bool {
    name.is_empty() || name.trim() != name
}

/// `owner.name`, each [`shown_name`], or the name alone without an owner.
pub fn shown_qualified(dialect: Dialect, o: &ObjectRef) -> String {
    let name = shown_name(dialect, &o.name);
    if o.owner.is_empty() {
        return name;
    }
    format!("{}.{name}", shown_name(dialect, &o.owner))
}

/// `name` as the SQL of `dialect` writes it: quoted where it needs it,
/// Oracle's plain names in lowercase, which read better. A quoted name
/// keeps its case, the dictionary's (#230).
pub fn written(dialect: Dialect, name: &str) -> String {
    let quoted = dialect.quote_ident(name);
    if dialect == Dialect::ORACLE && quoted == name {
        quoted.to_lowercase()
    } else {
        quoted
    }
}

/// `owner.name`, each [`written`].
pub fn written_qualified(dialect: Dialect, owner: &str, name: &str) -> String {
    format!("{}.{}", written(dialect, owner), written(dialect, name))
}

/// A bind for each column, named after it: in lowercase, `_` for what a
/// bind can't hold (it ends at a space or `-`), `c` before a name that
/// doesn't start with a letter, as a bind must, and a number where two
/// columns would share one (#230).
fn bind_names(cols: &[TableColumn]) -> Vec<String> {
    let mut binds: Vec<String> = Vec::new();
    for c in cols {
        let mut name: String = c
            .name
            .to_lowercase()
            .chars()
            .map(|ch| if ch.is_alphanumeric() { ch } else { '_' })
            .collect();
        if !name.starts_with(char::is_alphabetic) {
            name.insert(0, 'c');
        }
        let mut bind = format!(":{name}");
        for n in 2.. {
            if !binds.contains(&bind) {
                break;
            }
            bind = format!(":{name}_{n}");
        }
        binds.push(bind);
    }
    binds
}

/// Builds the statement for `kind` with binds named after the columns.
/// Oracle's names are written in lowercase; MySQL's keep their case.
fn template(dialect: Dialect, kind: Template, object: &ObjectRef, cols: &[TableColumn]) -> String {
    let name = |c: &TableColumn| written(dialect, &c.name);
    let table = written_qualified(dialect, &object.owner, &object.name);
    let names: Vec<String> = cols.iter().map(name).collect();
    let binds = bind_names(cols);
    match kind {
        Template::ColumnList => names.join(", "),
        Template::Select => {
            let alias = object
                .name
                .chars()
                .next()
                .filter(char::is_ascii_alphabetic)
                .map_or('t', |c| c.to_ascii_lowercase());
            let list: Vec<String> = names.iter().map(|n| format!("{alias}.{n}")).collect();
            format!("select {}\n  from {table} {alias}", list.join(",\n       "))
        }
        Template::Insert => format!(
            "insert into {table}\n  ({})\nvalues\n  ({})",
            names.join(", "),
            binds.join(", ")
        ),
        Template::Update => {
            let columns: Vec<(&TableColumn, &String)> = cols.iter().zip(&binds).collect();
            let mut keys: Vec<_> = columns
                .iter()
                .filter(|(c, _)| c.pk_position.is_some())
                .collect();
            keys.sort_by_key(|(c, _)| c.pk_position);
            let sets: Vec<String> = columns
                .iter()
                .filter(|(c, _)| c.pk_position.is_none())
                .map(|(c, bind)| format!("{} = {bind}", name(c)))
                .collect();
            let wheres: Vec<String> = if keys.is_empty() {
                vec!["1 = 0 -- TODO: no primary key".into()]
            } else {
                keys.iter()
                    .map(|(c, bind)| format!("{} = {bind}", name(c)))
                    .collect()
            };
            format!(
                "update {table}\n   set {}\n where {}",
                sets.join(",\n       "),
                wheres.join("\n   and ")
            )
        }
    }
}

fn object_ref(o: &DbObject) -> ObjectRef {
    ObjectRef {
        owner: o.owner.clone(),
        name: o.name.clone(),
        object_type: o.object_type,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test… opens the block in a SQL window; Debug… stops in the body
    /// (#85). Which overload's block it is: `test_script`'s live test.
    #[test]
    fn test_block_opens_or_debugs() {
        let browser = Browser {
            connection: Some("hr@db".into()),
            ..Browser::default()
        };
        let package = ObjectRef {
            owner: "HR".into(),
            name: "PKG".into(),
            object_type: ObjectType::Package,
        };
        let open = |debug| {
            let script = crate::test_script::Script {
                block: "begin hr.pkg.p(p_b => :p_b); end;".into(),
                subprogram: "P".into(),
            };
            let mut effects = Effects::default();
            browser.open_test(script, &package, debug, &mut effects);
            match effects.open.pop() {
                Some(OpenRequest::Sql { text, .. }) => (text, None),
                Some(OpenRequest::Debug { text, entry, .. }) => (text, Some(entry)),
                _ => panic!("no test window"),
            }
        };
        assert_eq!(
            open(false),
            ("begin hr.pkg.p(p_b => :p_b); end;".into(), None)
        );
        let (text, entry) = open(true);
        assert!(text.contains("p_b => :p_b"), "{text}");
        let body = pythia_debugger::Unit::new("HR", "PKG", pythia_debugger::UnitType::PackageBody);
        assert_eq!(entry, Some((body, "P".to_owned())));
    }

    /// The schema list has Oracle's PUBLIC, which owns public synonyms and
    /// links; MySQL has no such database (#83).
    #[test]
    fn public_schema_on_oracle_only() {
        let schemas = |dialect: Dialect| {
            let mut browser = Browser {
                dialect,
                ..Browser::default()
            };
            let mut effects = Effects::default();
            browser.meta(&mut effects, Pending::Schemas, MetaQuery::Schemas);
            let (_, DbRequest::Meta { id, .. }) = &effects.requests[0] else {
                panic!("no schema request");
            };
            let result = Ok(MetaResult::Schemas(vec!["HR".into(), "SCOTT".into()]));
            browser.on_event(DbEvent::Meta { id: *id, result }, &mut Vec::new());
            match browser.schemas {
                Some(Loaded::Ready(s)) => s,
                _ => panic!("schemas not loaded"),
            }
        };
        assert_eq!(schemas(Dialect::ORACLE), ["HR", "PUBLIC", "SCOTT"]);
        assert_eq!(schemas(Dialect::MYSQL), ["HR", "SCOTT"]);
    }

    /// A MySQL connection without a database starts in the one last picked,
    /// if it's still there; one from the profile wins (#133).
    #[test]
    fn opens_the_database_last_picked() {
        // Connects with `current` as the database and loads the list.
        let connect = |marks: Marks, current: &str| {
            let mut browser = Browser {
                marks,
                ..Browser::default()
            };
            let effects = browser.attach("c".into(), current.into(), Dialect::MYSQL, false);
            let (_, DbRequest::Meta { id, .. }) = &effects.requests[0] else {
                panic!("no schema request");
            };
            let result = Ok(MetaResult::Schemas(vec!["shop".into(), "sys".into()]));
            let mut log = Vec::new();
            let effects = browser.on_event(DbEvent::Meta { id: *id, result }, &mut log);
            (browser, effects.database, log)
        };

        let mut browser = Browser::default();
        browser.attach("c".into(), String::new(), Dialect::MYSQL, false);
        browser.set_schema("shop".into(), &mut Effects::default());
        let marks = browser.marks;
        assert_eq!(marks.databases["c"], "shop");

        let (browser, database, log) = connect(marks.clone(), "");
        assert_eq!(browser.schema.as_deref(), Some("shop"));
        assert_eq!(database.as_deref(), Some("shop"), "sessions switch too");
        assert!(log.is_empty(), "{log:?}");

        let (browser, database, _) = connect(marks.clone(), "sys");
        assert_eq!(browser.schema.as_deref(), Some("sys"), "the profile's");
        assert_eq!(database, None);
        assert_eq!(browser.marks.databases["c"], "shop", "kept for next time");

        let mut gone = marks;
        gone.databases.insert("c".into(), "old".into());
        let (browser, database, log) = connect(gone, "");
        assert_eq!(browser.schema.as_deref(), Some(""));
        assert_eq!(database, None);
        assert_eq!(log, ["c: database old, last used, isn't there anymore"]);
        assert!(browser.marks.databases.is_empty(), "forgotten");
    }

    #[test]
    fn drops_public_objects_and_links() {
        let o = |owner: &str, name: &str, object_type| ObjectRef {
            owner: owner.into(),
            name: name.into(),
            object_type,
        };
        assert_eq!(
            drop_sql(Dialect::ORACLE, &o("PUBLIC", "DUAL2", ObjectType::Synonym)),
            "drop public synonym DUAL2"
        );
        assert_eq!(
            drop_sql(
                Dialect::ORACLE,
                &o("PUBLIC", "REMOTE.EXAMPLE.COM", ObjectType::DatabaseLink)
            ),
            "drop public database link \"REMOTE.EXAMPLE.COM\""
        );
        assert_eq!(
            drop_sql(
                Dialect::ORACLE,
                &o("HR", "REMOTE", ObjectType::DatabaseLink)
            ),
            "drop database link REMOTE"
        );
        assert_eq!(
            drop_sql(Dialect::ORACLE, &o("HR", "EMP", ObjectType::Table)),
            "drop table HR.EMP"
        );
        // There's no DROP JOB (#46).
        assert_eq!(
            drop_sql(Dialect::ORACLE, &o("HR", "NIGHTLY", ObjectType::Job)),
            "begin dbms_scheduler.drop_job('HR.NIGHTLY', force => true); end;"
        );
        assert_eq!(
            folders(Dialect::ORACLE, false, "PUBLIC"),
            ObjectType::PUBLIC
        );
    }

    /// MariaDB's sequences have a folder of their own; MySQL has none
    /// (#270).
    #[test]
    fn mariadb_has_sequences() {
        let has =
            |mariadb| folders(Dialect::MYSQL, mariadb, "shop").contains(&ObjectType::Sequence);
        assert!(has(true));
        assert!(!has(false));
        assert_eq!(
            folders(Dialect::MYSQL, true, "shop")[..5],
            *Dialect::MYSQL.browser_types()
        );
    }

    /// A PostgreSQL function of several argument lists is dropped whole,
    /// as the browser lists it (#195): `drop function name` refused it.
    /// A procedure of the same name stays.
    #[test]
    fn drops_every_postgres_overload() {
        let o = |name: &str, object_type| ObjectRef {
            owner: "public".into(),
            name: name.into(),
            object_type,
        };
        let sql = drop_sql(Dialect::POSTGRES, &o("summa", ObjectType::Function));
        assert!(
            sql.contains("p.proname = 'summa' and p.prokind = 'f'"),
            "{sql}"
        );
        assert!(
            sql.contains("execute format('drop function %s', r)"),
            "{sql}"
        );
        let dropped = crate::test_db::run_on(
            "PYTHIA_TEST_POSTGRES",
            pythia_config::DbKind::Postgres,
            move |c| {
                for sql in [
                    "drop function if exists pythia_over(integer, integer)",
                    "drop function if exists pythia_over(numeric, numeric, numeric)",
                    "drop procedure if exists pythia_over(text)",
                    "create function pythia_over(a integer, b integer) returns integer \
                 language sql as $$ select a + b $$",
                    "create function pythia_over(a numeric, b numeric, c numeric) returns numeric \
                 language sql as $$ select a + b + c $$",
                    "create procedure pythia_over(t text) language sql as $$ select t $$",
                ] {
                    c.execute(sql).unwrap();
                }
                let left = |c: &mut dyn pythia_db::Client| {
                    c.query(
                        "select string_agg(prokind::text, '' order by prokind) from pg_proc \
                     where proname = 'pythia_over'",
                        &[],
                    )
                    .unwrap()[0][0]
                        .clone()
                };
                assert_eq!(left(c).as_deref(), Some("ffp"));
                c.execute(&drop_sql(
                    Dialect::POSTGRES,
                    &o("pythia_over", ObjectType::Function),
                ))
                .unwrap();
                let after_functions = left(c);
                c.execute(&drop_sql(
                    Dialect::POSTGRES,
                    &o("pythia_over", ObjectType::Procedure),
                ))
                .unwrap();
                (after_functions, left(c))
            },
        );
        if let Some((after_functions, after_all)) = dropped {
            assert_eq!(after_functions.as_deref(), Some("p"));
            assert_eq!(after_all, None);
        }
    }

    #[test]
    fn drops_users_and_roles() {
        let account = |name: &str, object_type| ObjectRef {
            owner: String::new(),
            name: name.into(),
            object_type,
        };
        assert_eq!(
            drop_sql(Dialect::ORACLE, &account("SCOTT", ObjectType::User)),
            "drop user SCOTT"
        );
        assert_eq!(
            drop_sql(Dialect::MYSQL, &account("app@localhost", ObjectType::User)),
            "drop user 'app'@'localhost'"
        );
        // The host is the last @'s.
        assert_eq!(
            drop_sql(Dialect::MYSQL, &account("o'neil@x@%", ObjectType::User)),
            "drop user 'o''neil@x'@'%'"
        );
        assert_eq!(
            drop_sql(Dialect::MYSQL, &account("reader", ObjectType::Role)),
            "drop role 'reader'"
        );
        assert_eq!(
            drop_sql(Dialect::POSTGRES, &account("App", ObjectType::User)),
            "drop user \"App\""
        );
        assert_eq!(
            drop_sql(Dialect::SQLSERVER, &account("reader", ObjectType::Role)),
            "drop role reader"
        );
        // The question and the log name them so (#268).
        for (dialect, name, named_so) in [
            (Dialect::ORACLE, "SCOTT", "SCOTT"),
            (Dialect::MYSQL, "app@localhost", "'app'@'localhost'"),
            (Dialect::POSTGRES, "App", "\"App\""),
            (Dialect::SQLSERVER, "reader", "reader"),
        ] {
            assert_eq!(named(dialect, &account(name, ObjectType::User)), named_so);
        }
    }

    fn col(name: &str, pk: Option<u32>) -> TableColumn {
        TableColumn {
            name: name.into(),
            data_type: "NUMBER".into(),
            nullable: pk.is_none(),
            default: None,
            comment: None,
            pk_position: pk,
        }
    }

    #[test]
    fn templates() {
        let object = ObjectRef {
            owner: "HR".into(),
            name: "EMP".into(),
            object_type: ObjectType::Table,
        };
        let cols = [col("ID", Some(1)), col("NAME", None), col("SAL", None)];
        let oracle = Dialect::ORACLE;
        assert_eq!(
            template(oracle, Template::ColumnList, &object, &cols),
            "id, name, sal"
        );
        assert_eq!(
            template(oracle, Template::Select, &object, &cols),
            "select e.id,\n       e.name,\n       e.sal\n  from hr.emp e"
        );
        assert_eq!(
            template(oracle, Template::Insert, &object, &cols),
            "insert into hr.emp\n  (id, name, sal)\nvalues\n  (:id, :name, :sal)"
        );
        assert_eq!(
            template(oracle, Template::Update, &object, &cols),
            "update hr.emp\n   set name = :name,\n       sal = :sal\n where id = :id"
        );

        // MySQL keeps the names' case and quotes with backticks.
        let object = ObjectRef {
            owner: "shop".into(),
            name: "Order".into(),
            object_type: ObjectType::Table,
        };
        let cols = [col("id", Some(1)), col("Total", None), col("key", None)];
        assert_eq!(
            template(Dialect::MYSQL, Template::Select, &object, &cols),
            "select o.id,\n       o.Total,\n       o.`key`\n  from shop.`Order` o"
        );
    }

    /// Names keep the case that tells them apart; Oracle's uppercase ones
    /// read in lowercase (#272).
    #[test]
    fn shown_names() {
        for (dialect, name, shown) in [
            (Dialect::ORACLE, "EMP", "emp"),
            (Dialect::ORACLE, "MY TABLE", "my table"),
            (Dialect::ORACLE, "Emp", "\"Emp\""),
            (Dialect::ORACLE, "emp", "\"emp\""),
            (Dialect::POSTGRES, "Emp", "Emp"),
            (Dialect::POSTGRES, "emp", "emp"),
            (Dialect::POSTGRES, "EMP", "EMP"),
            (Dialect::MYSQL, "Emp", "Emp"),
            (Dialect::SQLSERVER, "Emp", "Emp"),
            (Dialect::SQLITE, "Emp", "Emp"),
            (Dialect::SQLITE, "", "\"\""),
            (Dialect::SQLITE, "  ", "\"  \""),
            (Dialect::MYSQL, "", "``"),
            (Dialect::SQLSERVER, " ", "[ ]"),
            (Dialect::SQLITE, " t", "\" t\""),
            (Dialect::SQLITE, "t ", "\"t \""),
            (Dialect::SQLITE, "a b", "a b"),
            (Dialect::POSTGRES, "\tt", "\"\tt\""),
            (Dialect::MYSQL, " t", "` t`"),
            (Dialect::SQLSERVER, "t ", "[t ]"),
            (Dialect::ORACLE, " T", "\" T\""),
            (Dialect::ORACLE, "A B", "a b"),
        ] {
            assert_eq!(shown_name(dialect, name), shown, "{dialect:?} {name}");
        }
        let emp = |owner: &str| ObjectRef {
            owner: owner.into(),
            name: "Emp".into(),
            object_type: ObjectType::Table,
        };
        assert_eq!(shown_qualified(Dialect::ORACLE, &emp("HR")), "hr.\"Emp\"");
        assert_eq!(
            shown_qualified(Dialect::POSTGRES, &emp("Sales")),
            "Sales.Emp"
        );
        assert_eq!(shown_qualified(Dialect::POSTGRES, &emp("")), "Emp");
    }

    /// The filter, uppercased, finds a name in any case: an Oracle name with
    /// lowercase letters too (#305).
    #[test]
    fn names_match_without_case() {
        for (name, filter) in [
            ("R62_Case", "CASE"),
            ("R62_CASE", "CASE"),
            ("r62_čaj", "ČAJ"),
            ("Straße", "STRASSE"),
            ("emp", ""),
        ] {
            assert!(name_matches(name, filter), "{name} {filter}");
        }
        assert!(!name_matches("R62_Case", "CASES"));
    }

    /// Names that need quotes give binds that are binds: a bind ends at a
    /// space or `-`, and starts with a letter. Two columns never share one.
    #[test]
    fn templates_of_quoted_names() {
        let object = ObjectRef {
            owner: "public".into(),
            name: "2024 Orders".into(),
            object_type: ObjectType::Table,
        };
        let cols = [
            col("Id", Some(1)),
            col("Order Date", None),
            col("_note", None),
            col("e-mail", None),
            col("id", None),
        ];
        let postgres = Dialect::POSTGRES;
        assert_eq!(
            template(postgres, Template::Insert, &object, &cols),
            "insert into public.\"2024 Orders\"\n  \
             (\"Id\", \"Order Date\", _note, \"e-mail\", id)\nvalues\n  \
             (:id, :order_date, :c_note, :e_mail, :id_2)"
        );
        assert_eq!(
            template(postgres, Template::Update, &object, &cols),
            "update public.\"2024 Orders\"\n   set \"Order Date\" = :order_date,\n       \
             _note = :c_note,\n       \"e-mail\" = :e_mail,\n       id = :id_2\n \
             where \"Id\" = :id"
        );
        assert_eq!(
            template(postgres, Template::Select, &object, &cols[..2]),
            "select t.\"Id\",\n       t.\"Order Date\"\n  from public.\"2024 Orders\" t"
        );
        // Each bind is one to the SQL window.
        let insert = template(postgres, Template::Insert, &object, &cols);
        let binds: Vec<&str> = pythia_plsql::tokenize_in(&insert, postgres.syntax())
            .into_iter()
            .filter(|t| t.kind == pythia_plsql::TokenKind::BindVar)
            .map(|t| &insert[t.span])
            .collect();
        assert_eq!(binds, [":id", ":order_date", ":c_note", ":e_mail", ":id_2"]);

        // Oracle's plain names read in lowercase, its quoted ones as they are.
        let object = ObjectRef {
            owner: "HR".into(),
            name: "Mixed".into(),
            object_type: ObjectType::Table,
        };
        let cols = [col("ID", Some(1)), col("Name", None)];
        assert_eq!(
            template(Dialect::ORACLE, Template::Select, &object, &cols),
            "select m.id,\n       m.\"Name\"\n  from hr.\"Mixed\" m"
        );
        assert_eq!(
            template(Dialect::ORACLE, Template::Update, &object, &cols),
            "update hr.\"Mixed\"\n   set \"Name\" = :name\n where id = :id"
        );
    }

    #[test]
    fn mysql_statements() {
        let routine = ObjectRef {
            owner: "shop".into(),
            name: "add_order".into(),
            object_type: ObjectType::Procedure,
        };
        assert_eq!(
            drop_sql(Dialect::MYSQL, &routine),
            "drop procedure shop.add_order"
        );
        let ddl = "CREATE DEFINER=`app`@`%` PROCEDURE `add_order`()\nbegin\n  select 1;\nend\n";
        let script = replace_script(Dialect::MYSQL, &routine, ddl, false);
        // In its database, whatever the current one (#248).
        assert_eq!(
            script,
            "use shop;\n\n\
             -- MySQL can't replace a procedure in place: the drop revokes the grants\n\
             -- on it, and if the create fails, the procedure is gone.\n\
             drop procedure if exists shop.add_order;\n\n\
             CREATE DEFINER=`app`@`%` PROCEDURE `add_order`()\nbegin\n  select 1;\nend\n/\n"
        );
        // Three statements: the use, the drop, and the whole routine up to
        // the slash.
        let statements = pythia_plsql::split_statements_in(&script, pythia_plsql::Syntax::MySql);
        assert_eq!(statements.len(), 3, "{statements:?}");
        assert!(statements[2].text(&script).ends_with("end"));

        // MariaDB replaces it in place, MySQL a view (#240): one
        // statement, nothing dropped before.
        let script = replace_script(Dialect::MYSQL, &routine, ddl, true);
        assert_eq!(
            script,
            "use shop;\n\nCREATE or replace DEFINER=`app`@`%` PROCEDURE `add_order`()\nbegin\n  \
             select 1;\nend\n/\n"
        );
        let statements = pythia_plsql::split_statements_in(&script, pythia_plsql::Syntax::MySql);
        assert_eq!(statements.len(), 2, "{statements:?}");
        let view = ObjectRef {
            owner: "shop".into(),
            name: "v".into(),
            object_type: ObjectType::View,
        };
        let ddl = "CREATE ALGORITHM=UNDEFINED DEFINER=`app`@`%` SQL SECURITY DEFINER VIEW `v` AS \
                   select 1 AS `a`;";
        for mariadb in [false, true] {
            let script = replace_script(Dialect::MYSQL, &view, ddl, mariadb);
            assert_eq!(
                script,
                "use shop;\n\nCREATE or replace ALGORITHM=UNDEFINED DEFINER=`app`@`%` SQL \
                 SECURITY DEFINER VIEW `v` AS select 1 AS `a`;\n"
            );
            let statements =
                pythia_plsql::split_statements_in(&script, pythia_plsql::Syntax::MySql);
            assert_eq!(statements.len(), 2, "{statements:?}");
        }
        let trigger = ObjectRef {
            owner: "shop".into(),
            name: "t".into(),
            object_type: ObjectType::Trigger,
        };
        let ddl =
            "CREATE DEFINER=`app`@`%` TRIGGER t BEFORE INSERT ON x FOR EACH ROW set new.n = 1";
        assert!(
            replace_script(Dialect::MYSQL, &trigger, ddl, false).starts_with(
                "use shop;\n\n\
                 -- MySQL can't replace a trigger in place: if the create fails, the\n\
                 -- trigger is gone.\ndrop trigger if exists shop.t;\n"
            )
        );
        assert!(
            replace_script(Dialect::MYSQL, &trigger, ddl, true)
                .starts_with("use shop;\n\nCREATE or replace DEFINER=`app`@`%` TRIGGER t ")
        );
    }

    #[test]
    fn new_submenu() {
        let o = |owner: &str, name: &str, object_type| ObjectRef {
            owner: owner.into(),
            name: name.into(),
            object_type,
        };
        let labels = |t: ObjectType| -> Vec<String> {
            new_entries(&o("HR", "EMP", t))
                .into_iter()
                .map(|(label, _)| label)
                .collect()
        };
        assert_eq!(
            labels(ObjectType::Table),
            [
                "Table…",
                "Table like this…",
                "Index on this table…",
                "Trigger on this table…",
                "View on this table…",
                "Synonym for this…"
            ]
        );
        assert_eq!(
            labels(ObjectType::View),
            [
                "View…",
                "Trigger on this view…",
                "View on this view…",
                "Synonym for this…"
            ]
        );
        assert_eq!(
            labels(ObjectType::Procedure),
            ["Procedure…", "Synonym for this…"]
        );
        assert_eq!(labels(ObjectType::JavaSource), Vec::<String>::new());
        let (_, action) = new_entries(&o("HR", "EMP", ObjectType::Table)).remove(2);
        assert_eq!(
            action,
            ObjectAction::NewFrom(ObjectType::Index, o("HR", "EMP", ObjectType::Table))
        );
    }

    #[test]
    fn postgres_and_sqlserver_statements() {
        let o = |name: &str, object_type| ObjectRef {
            owner: "app".into(),
            name: name.into(),
            object_type,
        };
        let function = "CREATE OR REPLACE FUNCTION app.f()\n RETURNS integer\n LANGUAGE sql\nAS $function$select 1; $function$\n;";
        assert_eq!(
            replace_script(
                Dialect::POSTGRES,
                &o("f", ObjectType::Function),
                function,
                false
            ),
            format!("{function}\n")
        );
        assert_eq!(
            replace_script(
                Dialect::POSTGRES,
                &o("t", ObjectType::Trigger),
                "CREATE TRIGGER t BEFORE INSERT ON app.x FOR EACH ROW EXECUTE FUNCTION app.f();",
                false
            ),
            "CREATE or replace TRIGGER t BEFORE INSERT ON app.x FOR EACH ROW EXECUTE FUNCTION app.f();\n"
        );
        // A materialized view: dropped and created in one block (#286).
        let view = "create materialized view app.\"Mv\" as\n SELECT 1 AS found;";
        let script = replace_script(
            Dialect::POSTGRES,
            &o("Mv", ObjectType::MaterializedView),
            view,
            false,
        );
        assert_eq!(
            script,
            format!(
                "-- One block: if the create fails, the materialized view stays as it was.\n\
                 do $pythia$\n#variable_conflict use_column\nbegin\n\
                 drop materialized view if exists app.\"Mv\";\n\n{view}\nend\n$pythia$;\n"
            )
        );
        assert_eq!(
            pythia_plsql::split_statements_in(&script, pythia_plsql::Syntax::Postgres).len(),
            1
        );
        let tagged = "create materialized view app.v as\n SELECT '$pythia$'::text AS t";
        let script = replace_script(
            Dialect::POSTGRES,
            &o("v", ObjectType::MaterializedView),
            tagged,
            false,
        );
        assert!(script.contains("do $pythia_$\n"), "{script}");
        assert!(script.ends_with("AS t;\nend\n$pythia_$;\n"), "{script}");
        let drop = drop_sql(Dialect::POSTGRES, &o("t", ObjectType::Trigger));
        assert!(drop.contains("g.tgname = 't'"), "{drop}");
        assert_eq!(
            pythia_plsql::split_statements_in(&drop, pythia_plsql::Syntax::Postgres).len(),
            1
        );
        assert_eq!(
            drop_sql(Dialect::POSTGRES, &o("Emp", ObjectType::Table)),
            "drop table app.\"Emp\""
        );

        let procedure = "-- adds\ncreate procedure dbo.p as\nbegin\n  select 1;\nend\nGO\n";
        let script = replace_script(
            Dialect::SQLSERVER,
            &o("p", ObjectType::Procedure),
            procedure,
            false,
        );
        assert_eq!(
            script,
            "-- adds\ncreate or alter procedure dbo.p as\nbegin\n  select 1;\nend\nGO\n"
        );
        assert_eq!(
            pythia_plsql::split_statements_in(&script, pythia_plsql::Syntax::TSql).len(),
            1
        );
        assert_eq!(
            drop_sql(Dialect::SQLSERVER, &o("my proc", ObjectType::Procedure)),
            "drop procedure app.[my proc]"
        );
        // Stored without its schema: in its own, not the default one (#248).
        let unqualified = |name: &str, object_type, ddl: &str| {
            replace_script(Dialect::SQLSERVER, &o(name, object_type), ddl, false)
        };
        assert_eq!(
            unqualified("p", ObjectType::Procedure, "CREATE PROC [p] AS select 1"),
            "CREATE or alter PROC app.[p] AS select 1\n"
        );
        assert_eq!(
            unqualified(
                "t",
                ObjectType::Trigger,
                "create trigger t on x after insert as select 1"
            ),
            "create or alter trigger app.t on app.x after insert as select 1\n"
        );
        assert_eq!(
            unqualified(
                "d",
                ObjectType::Trigger,
                "create trigger d on database for create_table as select 1"
            ),
            "create or alter trigger app.d on database for create_table as select 1\n"
        );

        let lite = |name: &str, object_type| ObjectRef {
            owner: "main".into(),
            name: name.into(),
            object_type,
        };
        // In a transaction of its own that an error rolls back (#286).
        let replaced = |kind: &str, drop: &str, create: &str| {
            format!(
                "-- A transaction of its own: if the create fails, the {kind} stays\n\
                 -- as it was.\n\
                 begin;\nwhenever sqlerror exit rollback\n\n{drop};\n\n{create}\n\ncommit;\n"
            )
        };
        let trigger = "CREATE TRIGGER t AFTER INSERT ON x BEGIN\n  update x set n = 1;\nEND;\n";
        let script = replace_script(
            Dialect::SQLITE,
            &lite("t", ObjectType::Trigger),
            trigger,
            false,
        );
        assert_eq!(
            script,
            replaced(
                "trigger",
                "drop trigger if exists main.t",
                trigger.trim_end()
            )
        );
        let statements: Vec<&str> =
            pythia_plsql::split_statements_in(&script, pythia_plsql::Syntax::Sqlite)
                .iter()
                .map(|s| s.text(&script))
                .collect();
        assert_eq!(
            statements,
            [
                "begin",
                "whenever sqlerror exit rollback",
                "drop trigger if exists main.t",
                "CREATE TRIGGER t AFTER INSERT ON x BEGIN\n  update x set n = 1;\nEND",
                "commit"
            ]
        );
        // A view as written, a line comment at its end: the `;` goes on a
        // line of its own (#206).
        let view = "CREATE VIEW v AS select 'foo' as a\nwhere 1\n    -- and 0; really";
        let script = replace_script(Dialect::SQLITE, &lite("v", ObjectType::View), view, false);
        assert_eq!(
            script,
            replaced("view", "drop view if exists main.v", &format!("{view}\n;"))
        );
        let script = format!("{script}select 1;");
        assert_eq!(
            pythia_plsql::split_statements_in(&script, pythia_plsql::Syntax::Sqlite).len(),
            6
        );
        assert_eq!(
            drop_sql(Dialect::SQLITE, &lite("order", ObjectType::Table)),
            "drop table main.\"order\""
        );
        // Of an attached database: created there, not in main (#248).
        let aux = |name: &str, object_type| ObjectRef {
            owner: "aux".into(),
            name: name.into(),
            object_type,
        };
        assert_eq!(
            replace_script(
                Dialect::SQLITE,
                &aux("t", ObjectType::Trigger),
                trigger,
                false
            ),
            replaced(
                "trigger",
                "drop trigger if exists aux.t",
                "CREATE TRIGGER aux.t AFTER INSERT ON x BEGIN\n  update x set n = 1;\nEND;"
            )
        );
        assert_eq!(
            replace_script(
                Dialect::SQLITE,
                &aux("i", ObjectType::Index),
                "CREATE UNIQUE INDEX IF NOT EXISTS \"i\" ON x (n)",
                false
            ),
            replaced(
                "index",
                "drop index if exists aux.i",
                "CREATE UNIQUE INDEX IF NOT EXISTS aux.\"i\" ON x (n);"
            )
        );
    }
}

/// Starts dragging `text` (for editors to insert) when `response` is
/// dragged, with the text following the pointer.
fn drag_text(ui: &Ui, response: &egui::Response, text: String) {
    if response.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), crate::editor::DroppedText(text.clone()));
    }
    if response.dragged()
        && let Some(pos) = ui.ctx().pointer_interact_pos()
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        egui::Area::new(response.id.with("drag"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos + egui::vec2(12.0, 4.0))
            .interactable(false)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new(text).font(crate::theme::object_names(ui.style())));
                });
            });
    }
}

#[cfg(test)]
mod marks_tests {
    use super::*;

    fn object(name: &str) -> ObjectRef {
        ObjectRef {
            owner: "HR".into(),
            name: name.into(),
            object_type: ObjectType::Table,
        }
    }

    #[test]
    fn recent_and_favorites() {
        let mut marks = Marks::default();
        for i in 0..20 {
            marks.touch("db", object(&format!("T{i}")));
        }
        marks.touch("db", object("T5"));
        let recent = &marks.recent["db"];
        assert_eq!(recent.len(), Marks::RECENT);
        assert_eq!(recent[0].name, "T5");
        assert_eq!(recent.iter().filter(|o| o.name == "T5").count(), 1);

        marks.toggle_favorite("db", object("B"));
        marks.toggle_favorite("db", object("A"));
        assert!(marks.is_favorite("db", &object("A")));
        assert!(!marks.is_favorite("other", &object("A")));
        assert_eq!(marks.favorites["db"][0].name, "A");
        marks.toggle_favorite("db", object("A"));
        assert!(!marks.is_favorite("db", &object("A")));
    }

    /// Disconnecting the connection the browser shows keeps every
    /// connection's favorites and recent objects (#136).
    #[test]
    fn kept_when_disconnecting() {
        let mut browser = Browser::default();
        browser.attach("db".into(), "HR".into(), Dialect::ORACLE, false);
        browser.marks.toggle_favorite("db", object("A"));
        browser.marks.touch("other", object("B"));
        browser.detach("db");
        assert_eq!(browser.connection, None);
        assert!(browser.marks.is_favorite("db", &object("A")));
        assert_eq!(browser.marks.recent["other"], [object("B")]);
    }
}
