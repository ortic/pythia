//! Object editors: forms that create or change sequences, views,
//! materialized views, synonyms, indexes, database links, users and roles,
//! and the grants of any object. Each loads the object (if it exists), shows
//! the statements the edits turn into and applies them on the window's
//! session. Procedures, functions, packages, types and triggers start from a
//! form as well: it writes their first source, which a program window takes
//! over (compiling creates them). On MySQL/MariaDB, PostgreSQL and SQL
//! Server only users and roles have a form (`accounts`), run through the
//! session's [`pythia_db::Client`].

mod account;
mod accounts;
mod db_link;
mod directory;
mod grants;
mod index;
mod job;
mod mview;
mod profile;
mod program;
mod sequence;
mod synonym;
mod trigger;
mod type_def;
mod view;

#[cfg(test)]
pub use account::AccountDef;
#[cfg(test)]
pub use db_link::LinkDef;
#[cfg(test)]
pub use directory::DirectoryDef;
#[cfg(test)]
pub use index::IndexDef;
#[cfg(test)]
pub use job::JobDef;
#[cfg(test)]
pub use profile::ProfileDef;
#[cfg(test)]
pub use program::{Mode, Param, ProgramDef, Subprogram};
#[cfg(test)]
pub use sequence::SequenceDef;
#[cfg(test)]
pub use trigger::TriggerDef;
#[cfg(test)]
pub use view::ViewDef;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::Hash;

use egui::{Color32, RichText, Ui};
use pythia_db::oracle::Connection;
use pythia_db::{DbError, DbEvent, DbRequest, Dialect, Feature, ObjectType};
use pythia_ui::icons::Icon;
use serde::{Deserialize, Serialize};

use crate::documents::{
    DocContext, Effects, ObjectRef, OpenRequest, Target, quote_ident, request_id,
};
use crate::editor::Editor;

/// What an editor works on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditTarget {
    New {
        owner: String,
        object_type: ObjectType,
        /// The object the new one is on or for: the table of an index or
        /// trigger, the source of a view, the target of a synonym.
        #[serde(default)]
        from: Option<ObjectRef>,
    },
    Edit(ObjectRef),
    Grants(ObjectRef),
}

impl EditTarget {
    fn owner(&self) -> &str {
        match self {
            EditTarget::New { owner, .. } => owner,
            EditTarget::Edit(o) | EditTarget::Grants(o) => &o.owner,
        }
    }

    fn object(&self) -> Option<&ObjectRef> {
        match self {
            EditTarget::New { .. } => None,
            EditTarget::Edit(o) | EditTarget::Grants(o) => Some(o),
        }
    }

    fn object_type(&self) -> ObjectType {
        match self {
            EditTarget::New { object_type, .. } => *object_type,
            EditTarget::Edit(o) | EditTarget::Grants(o) => o.object_type,
        }
    }
}

/// Types whose existing objects have a form editor (grants work for any
/// object).
pub fn has_editor(t: ObjectType) -> bool {
    matches!(
        t,
        ObjectType::Sequence
            | ObjectType::View
            | ObjectType::MaterializedView
            | ObjectType::Synonym
            | ObjectType::Index
            | ObjectType::DatabaseLink
            | ObjectType::Job
            | ObjectType::Directory
            | ObjectType::User
            | ObjectType::Role
            | ObjectType::Profile
    )
}

/// Whether objects of type `t` have a form (to edit and create them) on
/// `dialect`'s databases.
pub fn has_form(dialect: Dialect, t: ObjectType) -> bool {
    match t {
        ObjectType::User | ObjectType::Role => dialect.supports(Feature::AccountEditors),
        t => has_editor(t) && dialect.supports(Feature::ObjectEditors),
    }
}

/// Types created from a form (tables from the table builder).
pub fn can_create(t: ObjectType) -> bool {
    has_editor(t)
        || matches!(
            t,
            ObjectType::Table
                | ObjectType::Trigger
                | ObjectType::Procedure
                | ObjectType::Function
                | ObjectType::Package
                | ObjectType::Type
        )
}

/// Types that take object grants.
pub fn has_grants(t: ObjectType) -> bool {
    matches!(
        t,
        ObjectType::Table
            | ObjectType::View
            | ObjectType::MaterializedView
            | ObjectType::Sequence
            | ObjectType::Package
            | ObjectType::Procedure
            | ObjectType::Function
            | ObjectType::Type
            | ObjectType::Library
            | ObjectType::Directory
    )
}

#[derive(Debug, Clone, PartialEq)]
enum Form {
    Sequence(sequence::SequenceDef),
    View(view::ViewDef),
    MaterializedView(mview::MviewDef),
    Synonym(synonym::SynonymDef),
    Index(index::IndexDef),
    DatabaseLink(db_link::LinkDef),
    Job(job::JobDef),
    Directory(directory::DirectoryDef),
    Account(account::AccountDef),
    /// A user or role of MySQL, PostgreSQL or SQL Server.
    Accounts(accounts::UserDef),
    Profile(profile::ProfileDef),
    Grants(grants::GrantsDef),
    Program(program::ProgramDef),
    Type(type_def::TypeDef),
    Trigger(trigger::TriggerDef),
}

impl Form {
    /// The object's name as the dictionary has it.
    fn name(&self) -> String {
        match self {
            Form::Sequence(d) => ident(&d.name),
            Form::View(d) => ident(&d.name),
            Form::MaterializedView(d) => ident(&d.name),
            Form::Synonym(d) => ident(&d.name),
            Form::Index(d) => ident(&d.name),
            Form::DatabaseLink(d) => ident(&d.name),
            Form::Job(d) => ident(&d.name),
            Form::Directory(d) => ident(&d.name),
            Form::Account(d) => ident(&d.name),
            Form::Accounts(d) => d.browser_name(),
            Form::Profile(d) => ident(&d.name),
            Form::Program(d) => ident(&d.name),
            Form::Type(d) => ident(&d.name),
            Form::Trigger(d) => ident(&d.effective_name()),
            Form::Grants(_) => String::new(),
        }
    }

    /// Who owns the object: the editor's schema, except for public
    /// synonyms and links (PUBLIC) and users, roles, profiles and
    /// directories (nobody).
    fn owner<'a>(&self, schema: &'a str) -> &'a str {
        match self {
            Form::Synonym(d) if d.public => "PUBLIC",
            Form::DatabaseLink(d) if d.public => "PUBLIC",
            Form::Account(_) | Form::Accounts(_) | Form::Profile(_) | Form::Directory(_) => "",
            _ => schema,
        }
    }

    /// A PL/SQL unit: the form writes its first source for a program
    /// window instead of statements to run.
    fn is_program(&self) -> bool {
        matches!(self, Form::Program(_) | Form::Type(_) | Form::Trigger(_))
    }

    /// The query of a view or materialized view, edited in a code editor.
    fn query(&self) -> Option<&String> {
        match self {
            Form::View(d) => Some(&d.query),
            Form::MaterializedView(d) => Some(&d.query),
            _ => None,
        }
    }

    fn query_mut(&mut self) -> Option<&mut String> {
        match self {
            Form::View(d) => Some(&mut d.query),
            Form::MaterializedView(d) => Some(&mut d.query),
            _ => None,
        }
    }
}

/// Names for the pickers.
#[derive(Default)]
pub struct Lists {
    /// Owner of the edited object.
    pub schema: String,
    pub release: pythia_db::compat::Release,
    pub owners: Vec<String>,
    /// Objects by owner, loaded when a picker needs them.
    pub objects: HashMap<String, Vec<String>>,
    pub tables: Vec<String>,
    pub views: Vec<String>,
    /// Columns by table (of `schema`).
    pub columns: HashMap<String, Vec<String>>,
    pub tablespaces: Vec<String>,
    pub temp_tablespaces: Vec<String>,
    pub db_links: Vec<String>,
    /// Procedures of `schema`, packaged ones as `pkg.proc`.
    pub procedures: Vec<String>,
    pub grantees: Vec<String>,
    pub profiles: Vec<String>,
    pub roles: Vec<String>,
    /// System privileges.
    pub privileges: Vec<String>,
    /// The session's system privileges, to say what's missing.
    pub session_privileges: Vec<String>,
    /// Password verify functions (owned by SYS).
    pub verify_functions: Vec<String>,
    /// Who the session is logged in as.
    pub session_user: String,
    /// Objects of the edited type in the schema, to warn before a new
    /// PL/SQL unit replaces one (`CREATE OR REPLACE`).
    pub existing: Vec<String>,
    /// The pickers of MySQL's, PostgreSQL's and SQL Server's users and roles.
    pub accounts: accounts::Choices,
    /// Lists a form asked for this frame.
    pub want_objects: Option<String>,
    pub want_columns: Option<String>,
    /// A job's next runs for a start date and repeat interval.
    pub want_next_runs: Option<(String, String)>,
    pub next_runs: HashMap<(String, String), Result<Vec<String>, String>>,
}

/// The lists loaded with the form.
#[derive(Default)]
struct BaseLists {
    release: pythia_db::compat::Release,
    owners: Vec<String>,
    tables: Vec<String>,
    views: Vec<String>,
    tablespaces: Vec<String>,
    temp_tablespaces: Vec<String>,
    db_links: Vec<String>,
    procedures: Vec<String>,
    grantees: Vec<String>,
    profiles: Vec<String>,
    roles: Vec<String>,
    privileges: Vec<String>,
    session_privileges: Vec<String>,
    verify_functions: Vec<String>,
    session_user: String,
    existing: Vec<String>,
    accounts: accounts::Choices,
}

/// The object as loaded (none for a new one), the form to start from, and
/// the lists.
type Loaded = (Option<Form>, Form, BaseLists);

enum ListRequest {
    Objects(String),
    Columns(String),
    NextRuns(String, String),
}

/// A statement that failed while applying.
#[derive(Debug)]
pub struct ApplyError {
    pub statement: String,
    pub error: DbError,
    /// A statement before the one that failed dropped the object, to
    /// create it again: it is gone now.
    pub dropped: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ObjectEditor {
    pub target: EditTarget,
    #[serde(skip)]
    state: State,
    /// The connection's database: Oracle's forms, or the user and role
    /// forms of the others.
    #[serde(skip)]
    dialect: Dialect,
}

#[derive(Default)]
struct State {
    loading: Option<u64>,
    lists_loading: BTreeMap<u64, ListRequest>,
    requested: BTreeSet<String>,
    original: Option<Form>,
    /// A new object's form as it started, to tell whether it was changed.
    initial: Option<Form>,
    edited: Option<Form>,
    lists: Lists,
    /// The query of a view or materialized view.
    query: Option<Editor>,
    applying: Option<u64>,
    load_error: Option<String>,
    /// Result of the last apply: message and success.
    outcome: Option<(String, bool)>,
    /// Text range of an apply error in the query editor.
    error_at: Option<std::ops::Range<usize>>,
}

impl ObjectEditor {
    pub fn new(target: EditTarget) -> Self {
        Self {
            target,
            state: State::default(),
            dialect: Dialect::default(),
        }
    }

    /// Loads the object from the document's (new) connection.
    pub fn attach(&mut self, dialect: Dialect) -> Effects {
        self.dialect = dialect;
        self.load()
    }

    pub fn title(&self) -> String {
        // The tab adds the unsaved-changes dot.
        let kind = self.target.object_type().dictionary_name().to_lowercase();
        // Oracle's names are uppercase; the others' as created.
        let name = |n: &str| {
            if self.dialect == Dialect::ORACLE {
                n.to_lowercase()
            } else {
                n.to_owned()
            }
        };
        match &self.target {
            EditTarget::New { .. } => format!("New {kind}"),
            EditTarget::Edit(o) => format!("{} ({kind})", name(&o.name)),
            EditTarget::Grants(o) => format!("{} grants", o.name.to_lowercase()),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.state.loading.is_some()
            || self.state.applying.is_some()
            || !self.state.lists_loading.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        match (&self.state.original, &self.state.edited) {
            (Some(a), Some(b)) => a != b,
            (None, Some(b)) => self.state.initial.as_ref() != Some(b),
            _ => false,
        }
    }

    #[cfg(test)]
    pub fn statements_for_test(&mut self) -> Result<Vec<String>, String> {
        self.sync_query();
        self.statements()
    }

    #[cfg(test)]
    pub fn outcome(&self) -> Option<(&str, bool)> {
        self.state.outcome.as_ref().map(|(m, ok)| (m.as_str(), *ok))
    }

    /// Changes the form in tests, e.g. `|f| f.cache = 50` on a sequence.
    #[cfg(test)]
    pub fn edit_for_test(&mut self, f: impl FnOnce(&mut dyn std::any::Any)) {
        let Some(form) = self.state.edited.as_mut() else {
            return;
        };
        match form {
            Form::Sequence(d) => f(d),
            Form::View(d) => f(d),
            Form::MaterializedView(d) => f(d),
            Form::Synonym(d) => f(d),
            Form::Index(d) => f(d),
            Form::DatabaseLink(d) => f(d),
            Form::Job(d) => f(d),
            Form::Directory(d) => f(d),
            Form::Account(d) => f(d),
            Form::Accounts(d) => f(d),
            Form::Profile(d) => f(d),
            Form::Grants(d) => f(d),
            Form::Program(d) => f(d),
            Form::Type(d) => f(d),
            Form::Trigger(d) => f(d),
        }
        if let (Some(query), Some(editor)) = (form.query(), self.state.query.as_mut()) {
            editor.set_text(query.clone());
        }
    }

    #[cfg(test)]
    pub fn apply_for_test(&mut self) -> Effects {
        self.apply()
    }

    /// "Open in editor" of a PL/SQL unit's form.
    #[cfg(test)]
    pub fn open_in_editor_for_test(&mut self, connection: &str) -> Effects {
        self.open_in_editor(Some(connection.to_owned()))
    }

    /// Loads the object and the picker lists.
    pub fn load(&mut self) -> Effects {
        let mut effects = Effects::default();
        let id = request_id();
        self.state.loading = Some(id);
        self.state.load_error = None;
        let target = self.target.clone();
        if self.dialect != Dialect::ORACLE {
            let task: pythia_db::RunFn = Box::new(move |client| {
                let loaded = load_account(client, &target)?;
                Ok(Box::new(loaded) as Box<dyn std::any::Any + Send>)
            });
            effects.request(Target::Meta, DbRequest::Run { id, task });
            return effects;
        }
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            let loaded = load(conn, &target)?;
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
                Ok(Ok(loaded)) => {
                    let (form, start, base) = *loaded;
                    self.state.query = start.query().map(|q| Editor::new(q.clone()));
                    self.state.original = form;
                    self.state.initial = Some(start.clone());
                    self.state.edited = Some(start);
                    let lists = &mut self.state.lists;
                    lists.schema = self.target.owner().to_owned();
                    lists.release = base.release;
                    lists.owners = base.owners;
                    lists.tables = base.tables;
                    lists.views = base.views;
                    lists.tablespaces = base.tablespaces;
                    lists.temp_tablespaces = base.temp_tablespaces;
                    lists.db_links = base.db_links;
                    lists.procedures = base.procedures;
                    lists.grantees = base.grantees;
                    lists.profiles = base.profiles;
                    lists.roles = base.roles;
                    lists.privileges = base.privileges;
                    lists.session_privileges = base.session_privileges;
                    lists.verify_functions = base.verify_functions;
                    lists.session_user = base.session_user;
                    lists.existing = base.existing;
                    lists.accounts = base.accounts;
                }
                Ok(Err(_)) => self.state.load_error = Some("unexpected result".into()),
                Err(e) => self.state.load_error = Some(e.to_string()),
            }
        } else if let Some(request) = self.state.lists_loading.remove(&id) {
            if let ListRequest::NextRuns(start, repeat) = request {
                let runs = match result.map(|any| any.downcast::<Vec<String>>()) {
                    Ok(Ok(runs)) => Ok(*runs),
                    Ok(Err(_)) => Err("unexpected result".to_owned()),
                    Err(e) => Err(format!("The repeat interval doesn't work: {}", e.text())),
                };
                self.state.lists.next_runs.insert((start, repeat), runs);
                return Effects::default();
            }
            let names = result
                .ok()
                .and_then(|any| any.downcast::<Vec<String>>().ok())
                .map(|n| *n)
                .unwrap_or_default();
            match request {
                ListRequest::Objects(owner) => {
                    self.state.lists.objects.insert(owner, names);
                }
                ListRequest::Columns(table) => {
                    self.state.lists.columns.insert(table, names);
                }
                ListRequest::NextRuns(..) => {}
            }
        } else if self.state.applying == Some(id) {
            self.state.applying = None;
            self.state.error_at = None;
            match result.map(|any| any.downcast::<Result<Vec<String>, ApplyError>>()) {
                Ok(Ok(outcome)) => match *outcome {
                    Ok(warnings) => {
                        let mut message = "Applied".to_owned();
                        for w in &warnings {
                            message.push('\n');
                            message.push_str(w);
                        }
                        self.state.outcome = Some((message, warnings.is_empty()));
                        if let EditTarget::New {
                            owner, object_type, ..
                        } = &self.target
                            && let Some(form) = &self.state.edited
                        {
                            self.target = EditTarget::Edit(ObjectRef {
                                owner: form.owner(owner).to_owned(),
                                name: form.name(),
                                object_type: *object_type,
                            });
                        }
                        let mut effects = self.load();
                        if let EditTarget::Edit(object) = &self.target {
                            effects.changed = Some(object.clone());
                        }
                        return effects;
                    }
                    Err(e) => {
                        self.locate_error(&e);
                        let mut message =
                            format!("{}\n\nin: {}", e.error.text(), first_line(&e.statement));
                        if e.dropped {
                            // Nothing to change any more: the next Apply
                            // creates it as the form has it.
                            message.push_str(
                                "\n\nIt was dropped to be created again, and isn't there now: \
                                 Apply creates it",
                            );
                            self.state.original = None;
                        }
                        self.state.outcome = Some((message, false));
                    }
                },
                Ok(Err(_)) => self.state.outcome = Some(("unexpected result".into(), false)),
                Err(e) => self.state.outcome = Some((e.to_string(), false)),
            }
        }
        Effects::default()
    }

    /// Selects the error position in the query editor.
    fn locate_error(&mut self, e: &ApplyError) {
        let (Some(offset), Some(query)) = (e.error.offset, self.state.query.as_mut()) else {
            return;
        };
        let Some(start) = e
            .statement
            .find(" as\n")
            .or_else(|| e.statement.find("\nas\n"))
            .map(|i| i + 4)
        else {
            return;
        };
        let Some(at) = (offset as usize).checked_sub(start) else {
            return;
        };
        let lead = query.text().len() - query.text().trim_start().len();
        let pos = (lead + at).min(query.text().len());
        query.select(pos..pos);
        self.state.error_at = Some(pos..pos);
    }

    fn sync_query(&mut self) {
        if let (Some(form), Some(q)) = (self.state.edited.as_mut(), &self.state.query)
            && let Some(query) = form.query_mut()
        {
            *query = q.text().to_owned();
        }
    }

    /// The statements to run, or a PL/SQL unit's first sources.
    fn statements(&self) -> Result<Vec<String>, String> {
        let Some(edited) = &self.state.edited else {
            return Ok(Vec::new());
        };
        let original = self.state.original.as_ref();
        if original == Some(edited) {
            return Ok(Vec::new());
        }
        let owner = self.target.owner();
        match (original, edited) {
            (o, Form::Sequence(d)) => {
                let o = o.and_then(|f| match f {
                    Form::Sequence(s) => Some(s),
                    _ => None,
                });
                sequence::statements(owner, o, d)
            }
            (_, Form::View(d)) => view::statements(owner, d),
            (o, Form::MaterializedView(d)) => {
                let o = o.and_then(|f| match f {
                    Form::MaterializedView(m) => Some(m),
                    _ => None,
                });
                mview::statements(owner, o, d)
            }
            (_, Form::Synonym(d)) => synonym::statements(owner, d),
            (o, Form::Index(d)) => {
                let o = o.and_then(|f| match f {
                    Form::Index(i) => Some(i),
                    _ => None,
                });
                index::statements(owner, o, d)
            }
            (o, Form::DatabaseLink(d)) => {
                let o = o.and_then(|f| match f {
                    Form::DatabaseLink(l) => Some(l),
                    _ => None,
                });
                db_link::statements(o, d)
            }
            (o, Form::Job(d)) => {
                let o = o.and_then(|f| match f {
                    Form::Job(j) => Some(j),
                    _ => None,
                });
                job::statements(owner, o, d)
            }
            (o, Form::Directory(d)) => {
                let o = o.and_then(|f| match f {
                    Form::Directory(d) => Some(d),
                    _ => None,
                });
                directory::statements(o, d)
            }
            (o, Form::Account(d)) => {
                let o = o.and_then(|f| match f {
                    Form::Account(a) => Some(a),
                    _ => None,
                });
                account::statements(o, d)
            }
            (o, Form::Accounts(d)) => {
                let o = o.and_then(|f| match f {
                    Form::Accounts(a) => Some(a),
                    _ => None,
                });
                accounts::statements(o, d)
            }
            (o, Form::Profile(d)) => {
                let o = o.and_then(|f| match f {
                    Form::Profile(p) => Some(p),
                    _ => None,
                });
                profile::statements(o, d)
            }
            (o, Form::Grants(d)) => {
                let empty = grants::GrantsDef::default();
                let o = match o {
                    Some(Form::Grants(g)) => g,
                    _ => &empty,
                };
                let object = match self.target.object() {
                    Some(o) if o.object_type == ObjectType::Directory => {
                        format!("directory {}", o.qualified())
                    }
                    Some(o) => o.qualified(),
                    None => String::new(),
                };
                Ok(grants::statements(&object, o, d))
            }
            (_, Form::Program(d)) => program::sources(owner, d),
            (_, Form::Type(d)) => type_def::sources(owner, d),
            (_, Form::Trigger(d)) => trigger::source(owner, d).map(|s| vec![s]),
        }
    }

    /// The statements as shown: passwords hidden.
    fn shown(&self, statements: &[String]) -> Vec<String> {
        match &self.state.edited {
            Some(Form::DatabaseLink(d)) => db_link::masked(statements, d),
            Some(Form::Account(d)) => account::masked(statements, d),
            Some(Form::Accounts(d)) => accounts::masked(statements, d),
            _ => statements.to_vec(),
        }
    }

    fn apply(&mut self) -> Effects {
        let mut effects = Effects::default();
        let Ok(statements) = self.statements() else {
            return effects;
        };
        if statements.is_empty() {
            return effects;
        }
        let view = match (&self.state.edited, self.target.object_type()) {
            (Some(form), ObjectType::View) => Some((self.target.owner().to_owned(), form.name())),
            _ => None,
        };
        let id = request_id();
        self.state.applying = Some(id);
        self.state.outcome = None;
        if self.dialect != Dialect::ORACLE {
            let task: pythia_db::RunFn = Box::new(move |client| {
                let outcome = apply_client(client, &statements);
                Ok(Box::new(outcome) as Box<dyn std::any::Any + Send>)
            });
            effects.request(Target::Own, DbRequest::Run { id, task });
            return effects;
        }
        let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
            let outcome = apply(conn, &statements, view);
            Ok(Box::new(outcome) as Box<dyn std::any::Any + Send>)
        });
        effects.request(Target::Own, DbRequest::Task { id, task });
        effects
    }

    /// Opens the unit's sources in a new program window and closes the form.
    fn open_in_editor(&mut self, connection: Option<String>) -> Effects {
        let mut effects = Effects::default();
        let (Ok(sources), Some(form), Some(connection)) =
            (self.statements(), &self.state.edited, connection)
        else {
            return effects;
        };
        if sources.is_empty() {
            return effects;
        }
        effects.open.push(OpenRequest::NewProgram {
            connection,
            object: ObjectRef {
                owner: self.target.owner().to_owned(),
                name: form.name(),
                object_type: self.target.object_type(),
            },
            sources,
        });
        effects.close = true;
        effects
    }

    /// Loads lists the form asked for.
    fn request_lists(&mut self, effects: &mut Effects) {
        let schema = self.state.lists.schema.clone();
        let wants = [
            self.state
                .lists
                .want_objects
                .take()
                .map(ListRequest::Objects),
            self.state
                .lists
                .want_columns
                .take()
                .map(ListRequest::Columns),
            self.state
                .lists
                .want_next_runs
                .take()
                .map(|(start, repeat)| ListRequest::NextRuns(start, repeat)),
        ];
        for want in wants.into_iter().flatten() {
            let key = match &want {
                ListRequest::Objects(o) => format!("objects:{o}"),
                ListRequest::Columns(t) => format!("columns:{t}"),
                ListRequest::NextRuns(start, repeat) => format!("next:{start}\u{1}{repeat}"),
            };
            if !self.state.requested.insert(key) {
                continue;
            }
            let id = request_id();
            if self.dialect != Dialect::ORACLE {
                // Only the user and role forms ask, for a picker's objects.
                let ListRequest::Objects(container) = &want else {
                    continue;
                };
                let container = container.clone();
                self.state.lists_loading.insert(id, want);
                let task: pythia_db::RunFn = Box::new(move |client| {
                    let names = accounts::objects(client, &container)?;
                    Ok(Box::new(names) as Box<dyn std::any::Any + Send>)
                });
                effects.request(Target::Meta, DbRequest::Run { id, task });
                continue;
            }
            if let ListRequest::NextRuns(start, repeat) = &want {
                let (start, repeat) = (start.clone(), repeat.clone());
                self.state.lists_loading.insert(id, want);
                let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
                    let runs = job::next_runs(conn, &start, &repeat)?;
                    Ok(Box::new(runs) as Box<dyn std::any::Any + Send>)
                });
                effects.request(Target::Meta, DbRequest::Task { id, task });
                continue;
            }
            let (sql, binds): (&str, Vec<String>) = match &want {
                ListRequest::Objects(owner) => (
                    "select object_name from all_objects
                      where owner = :1 and object_type in ('TABLE', 'VIEW', 'MATERIALIZED VIEW',
                            'SEQUENCE', 'SYNONYM', 'PACKAGE', 'PROCEDURE', 'FUNCTION', 'TYPE')
                      order by object_name",
                    vec![owner.clone()],
                ),
                ListRequest::Columns(table) => (
                    "select column_name from all_tab_columns
                      where owner = :1 and table_name = :2 order by column_id",
                    vec![schema.clone(), table.clone()],
                ),
                ListRequest::NextRuns(..) => unreachable!("sent above"),
            };
            self.state.lists_loading.insert(id, want);
            let task: pythia_db::TaskFn = Box::new(move |conn: &Connection| {
                let refs: Vec<&dyn pythia_db::oracle::sql_type::ToSql> = binds
                    .iter()
                    .map(|b| b as &dyn pythia_db::oracle::sql_type::ToSql)
                    .collect();
                let names = names(conn, sql, &refs)?;
                Ok(Box::new(names) as Box<dyn std::any::Any + Send>)
            });
            effects.request(Target::Meta, DbRequest::Task { id, task });
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        if cx.dialect != self.dialect {
            // Restored with the workspace before its connection said what
            // it is.
            return self.attach(cx.dialect);
        }
        if let Some(e) = &self.state.load_error {
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
        self.sync_query();
        let statements = self.statements();
        let existing = self.state.original.is_some();
        let program = self.state.edited.as_ref().is_some_and(Form::is_program);
        ui.horizontal(|ui| {
            let t = self.target.object_type();
            if let Some(icon) = Icon::for_object_type(t.dictionary_name()) {
                ui.add(icon.sized(ui, pythia_ui::icons::IconSize::Small));
            }
            let kind = t.dictionary_name().to_lowercase();
            let heading = match &self.target {
                EditTarget::New { owner, .. } if owner.is_empty() => format!("New {kind}"),
                EditTarget::New { owner, .. } if owner == "PUBLIC" => format!("New public {kind}"),
                EditTarget::New { owner, .. } => format!("New {kind} in {owner}"),
                EditTarget::Edit(o) if o.owner.is_empty() => {
                    let mut kind = kind.clone();
                    kind[..1].make_ascii_uppercase();
                    format!("{kind} {}", o.name)
                }
                EditTarget::Edit(o) => format!("{}.{}", o.owner, o.name),
                EditTarget::Grants(o) if o.owner.is_empty() => {
                    format!("Grants on {kind} {}", o.name)
                }
                EditTarget::Grants(o) => format!("Grants on {}.{}", o.owner, o.name),
            };
            ui.label(RichText::new(heading).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = matches!(&statements, Ok(s) if !s.is_empty());
                if program {
                    let open = egui::Button::image_and_text(Icon::Edit.image(ui), "Open in editor");
                    if ui
                        .add_enabled(ready && cx.connection.is_some(), open)
                        .on_hover_text("Continue in a program window, where compiling creates it")
                        .clicked()
                    {
                        effects.extend(self.open_in_editor(cx.connection.clone()));
                    }
                    return;
                }
                let apply = egui::Button::image_and_text(Icon::Execute.image(ui), "Apply");
                if ui
                    .add_enabled(ready && cx.connected && !self.is_busy(), apply)
                    .on_hover_text("Run the statements below in this window's session")
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
                    && let (Some(connection), Ok(s)) = (cx.connection.clone(), &statements)
                {
                    effects.open.push(OpenRequest::Sql {
                        connection,
                        text: script(s),
                        run: false,
                    });
                }
                if existing {
                    if ui
                        .add_enabled(self.is_dirty(), egui::Button::new("Revert"))
                        .clicked()
                    {
                        self.state.edited = self.state.original.clone();
                        if let (Some(query), Some(q)) = (
                            self.state.edited.as_ref().and_then(Form::query),
                            self.state.query.as_mut(),
                        ) {
                            q.set_text(query.clone());
                        }
                    }
                    if ui.button("Reload").clicked() {
                        effects.extend(self.load());
                    }
                }
            });
        });
        ui.separator();
        if let Some(form) = self.state.edited.as_ref().filter(|f| f.is_program()) {
            let name = form.name();
            if self.state.lists.existing.contains(&name) {
                let kind = self.target.object_type().dictionary_name().to_lowercase();
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("{kind} {name} exists: compiling the new source replaces it"),
                );
            }
        }
        if let Some((message, ok)) = &self.state.outcome {
            let color = if *ok {
                Color32::from_rgb(0x2f, 0xa0, 0x4c)
            } else {
                ui.visuals().error_fg_color
            };
            ui.label(RichText::new(message).color(color));
            ui.add_space(4.0);
        }

        // The DDL preview keeps a fixed share at the bottom.
        let preview_height = (ui.available_height() * 0.3).clamp(90.0, 220.0);
        let form_height = (ui.available_height() - preview_height - 30.0).max(120.0);
        let owner = self.target.owner().to_owned();
        let original = self.state.original.as_ref();
        let lists = &mut self.state.lists;
        let editor_id = egui::Id::new(("object-editor-query", cx.id));
        let query = &mut self.state.query;
        ui.allocate_ui(
            egui::vec2(ui.available_width(), form_height),
            |ui| match self.state.edited.as_mut().expect("loaded") {
                form @ (Form::View(_) | Form::MaterializedView(_)) => {
                    match form {
                        Form::View(def) => view::ui(ui, def, existing),
                        Form::MaterializedView(def) => {
                            let original = original.and_then(|f| match f {
                                Form::MaterializedView(m) => Some(m),
                                _ => None,
                            });
                            mview::ui(ui, def, original, lists)
                        }
                        _ => {}
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("Query").strong());
                    if let Some(query) = query.as_mut() {
                        let height = ui.available_height().max(80.0);
                        ui.allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
                            query.ui(ui, editor_id, &cx.settings.editor);
                        });
                    }
                }
                form => {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| match form {
                            Form::Sequence(def) => sequence::ui(ui, def, existing),
                            Form::Synonym(def) => synonym::ui(ui, def, existing, lists),
                            Form::Index(def) => index::ui(ui, def, existing, lists),
                            Form::DatabaseLink(def) => db_link::ui(ui, def, existing, &owner),
                            Form::Job(def) => job::ui(ui, def, existing, &owner, lists),
                            Form::Directory(def) => directory::ui(ui, def, existing, lists),
                            Form::Account(def) => {
                                let original = original.and_then(|f| match f {
                                    Form::Account(a) => Some(a),
                                    _ => None,
                                });
                                account::ui(ui, def, original, lists)
                            }
                            Form::Accounts(def) => {
                                let original = original.and_then(|f| match f {
                                    Form::Accounts(a) => Some(a),
                                    _ => None,
                                });
                                accounts::ui(ui, def, original, lists)
                            }
                            Form::Profile(def) => profile::ui(ui, def, existing, lists),
                            Form::Grants(def) => grants::ui(ui, def, lists),
                            Form::Program(def) => program::ui(ui, def),
                            Form::Type(def) => type_def::ui(ui, def),
                            Form::Trigger(def) => trigger::ui(ui, def, lists),
                            Form::View(_) | Form::MaterializedView(_) => {}
                        })
                        .inner
                }
            },
        );
        self.request_lists(&mut effects);

        ui.separator();
        ui.label(RichText::new(if program { "Source" } else { "Statements" }).strong());
        match &statements {
            Err(problem) => {
                ui.label(RichText::new(problem).color(ui.visuals().warn_fg_color));
            }
            Ok(s) if s.is_empty() => {
                ui.weak("No changes");
            }
            Ok(s) => {
                let shown = self.shown(s);
                let text = if program {
                    shown.join("\n/\n\n") + "\n/"
                } else {
                    script(&shown)
                };
                let theme = pythia_ui::SyntaxTheme::for_visuals(ui.visuals());
                let font = egui::FontId::monospace(cx.settings.editor.font_size.min(14.0));
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .corner_radius(4.0)
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        egui::ScrollArea::both()
                            .id_salt("ddl")
                            .auto_shrink([false, true])
                            .max_height(ui.available_height())
                            .show(ui, |ui| {
                                let job = pythia_ui::highlight(&text, font, &theme);
                                ui.add(egui::Label::new(job).extend());
                            });
                    });
            }
        }
        effects
    }
}

/// A new object's form, filled in from the object it's on or for.
fn new_form(conn: &Connection, target: &EditTarget) -> Result<Form, DbError> {
    let EditTarget::New {
        owner,
        object_type,
        from,
    } = target
    else {
        return Err(DbError::new("Not a new object".to_owned()));
    };
    let from = from.as_ref();
    Ok(match object_type {
        ObjectType::Sequence => Form::Sequence(sequence::SequenceDef::new()),
        ObjectType::View => {
            let mut def = view::ViewDef::new();
            if let Some(from) = from {
                let columns = names(
                    conn,
                    "select column_name from all_tab_columns
                      where owner = :1 and table_name = :2 order by column_id",
                    &[&from.owner, &from.name],
                )?;
                def.name = quote_ident(&format!("{}_V", from.name));
                def.query = view::query_on(owner, from, &columns);
            }
            Form::View(def)
        }
        ObjectType::MaterializedView => Form::MaterializedView(mview::MviewDef::new()),
        ObjectType::Synonym => {
            let mut def = synonym::SynonymDef {
                public: owner == "PUBLIC",
                ..Default::default()
            };
            if let Some(from) = from {
                def.name = quote_ident(&from.name);
                def.target_owner = quote_ident(&from.owner);
                def.target_name = quote_ident(&from.name);
                // In the object's own schema the name is taken: a public
                // synonym is what's usually wanted.
                def.public |= from.owner == *owner;
            }
            Form::Synonym(def)
        }
        ObjectType::Index => {
            let mut def = index::IndexDef::new();
            if let Some(from) = from {
                def.table = quote_ident(&from.name);
                def.name = quote_ident(&format!("{}_IX", from.name));
            }
            Form::Index(def)
        }
        ObjectType::DatabaseLink => Form::DatabaseLink(db_link::LinkDef {
            public: owner == "PUBLIC",
            ..Default::default()
        }),
        ObjectType::Job => Form::Job(job::JobDef::default()),
        ObjectType::Directory => Form::Directory(directory::DirectoryDef::default()),
        ObjectType::User => Form::Account(account::AccountDef::new_user()),
        ObjectType::Role => Form::Account(account::AccountDef::new_role()),
        ObjectType::Profile => Form::Profile(profile::new_def(conn)),
        t @ (ObjectType::Procedure | ObjectType::Function | ObjectType::Package) => {
            Form::Program(program::ProgramDef::new(*t))
        }
        ObjectType::Type => Form::Type(type_def::TypeDef::new()),
        ObjectType::Trigger => Form::Trigger(match from {
            Some(from) => trigger::TriggerDef::on(&from.name, from.object_type == ObjectType::View),
            None => trigger::TriggerDef::default(),
        }),
        t => {
            return Err(DbError::new(format!(
                "No form for a new {}",
                t.dictionary_name().to_lowercase()
            )));
        }
    })
}

fn tablespaces(conn: &Connection) -> Result<Vec<String>, DbError> {
    names(
        conn,
        "select tablespace_name from user_tablespaces
          where contents = 'PERMANENT' order by tablespace_name",
        &[],
    )
}

fn load(conn: &Connection, target: &EditTarget) -> Result<Loaded, DbError> {
    let owner = target.owner();
    let mut base = BaseLists {
        release: pythia_db::compat::release(conn)?,
        ..BaseLists::default()
    };
    if let EditTarget::Grants(o) = target {
        base.grantees = grantees(conn)?;
        let form = Form::Grants(grants::load(conn, &o.owner, &o.name, o.object_type)?);
        return Ok((Some(form.clone()), form, base));
    }
    match target.object_type() {
        ObjectType::Synonym => {
            base.owners = names(
                conn,
                "select username from all_users order by username",
                &[],
            )?;
            base.db_links = names(
                conn,
                "select db_link from all_db_links order by db_link",
                &[],
            )?;
        }
        ObjectType::Index => {
            base.tables = names(
                conn,
                "select table_name from all_tables where owner = :1 order by table_name",
                &[&owner],
            )?;
            base.tablespaces = tablespaces(conn)?;
        }
        ObjectType::MaterializedView => base.tablespaces = tablespaces(conn)?,
        ObjectType::Trigger => {
            base.tables = names(
                conn,
                "select table_name from all_tables where owner = :1 order by table_name",
                &[&owner],
            )?;
            base.views = names(
                conn,
                "select view_name from all_views where owner = :1 order by view_name",
                &[&owner],
            )?;
        }
        ObjectType::User | ObjectType::Role => account::lists(conn, &mut base)?,
        ObjectType::Profile => profile::lists(conn, &mut base)?,
        ObjectType::Job => job::lists(conn, owner, &mut base)?,
        ObjectType::Directory => directory::lists(conn, &mut base)?,
        _ => {}
    }
    if let EditTarget::New { object_type, .. } = target
        && matches!(
            object_type,
            ObjectType::Procedure
                | ObjectType::Function
                | ObjectType::Package
                | ObjectType::Type
                | ObjectType::Trigger
        )
    {
        base.existing = names(
            conn,
            "select object_name from all_objects where owner = :1 and object_type = :2",
            &[&owner, &object_type.dictionary_name()],
        )?;
    }
    let original = match target.object() {
        None => None,
        Some(o) => Some(match o.object_type {
            ObjectType::Sequence => Form::Sequence(sequence::load(conn, owner, &o.name)?),
            ObjectType::View => Form::View(view::load(conn, owner, &o.name)?),
            ObjectType::MaterializedView => {
                Form::MaterializedView(mview::load(conn, owner, &o.name)?)
            }
            ObjectType::Synonym => Form::Synonym(synonym::load(conn, owner, &o.name)?),
            ObjectType::Index => Form::Index(index::load(conn, owner, &o.name)?),
            ObjectType::DatabaseLink => Form::DatabaseLink(db_link::load(conn, owner, &o.name)?),
            ObjectType::Job => Form::Job(job::load(conn, owner, &o.name)?),
            ObjectType::Directory => Form::Directory(directory::load(conn, &o.name)?),
            ObjectType::User => Form::Account(account::load_user(conn, &o.name)?),
            ObjectType::Role => Form::Account(account::load_role(conn, &o.name)?),
            ObjectType::Profile => Form::Profile(profile::load(conn, &o.name)?),
            t => {
                return Err(DbError::new(format!(
                    "No editor for {}",
                    t.dictionary_name().to_lowercase()
                )));
            }
        }),
    };
    let start = match &original {
        Some(form) => form.clone(),
        None => new_form(conn, target)?,
    };
    Ok((original, start, base))
}

/// A user or role of MySQL, PostgreSQL or SQL Server, and its pickers.
fn load_account(
    client: &mut dyn pythia_db::Client,
    target: &EditTarget,
) -> Result<Loaded, DbError> {
    if !matches!(target.object_type(), ObjectType::User | ObjectType::Role) {
        return Err(DbError::new(format!(
            "No form for a {} on {}",
            target.object_type().dictionary_name().to_lowercase(),
            client.dialect().name()
        )));
    }
    let (original, start, choices) = accounts::load(client, target)?;
    let base = BaseLists {
        accounts: choices,
        ..BaseLists::default()
    };
    Ok((original.map(Form::Accounts), Form::Accounts(start), base))
}

/// Runs the statements through the session's client, stopping at the
/// first error.
fn apply_client(
    client: &mut dyn pythia_db::Client,
    statements: &[String],
) -> Result<Vec<String>, ApplyError> {
    for s in statements {
        client.execute(s).map_err(|error| ApplyError {
            statement: s.clone(),
            error,
            dropped: false,
        })?;
    }
    Ok(Vec::new())
}

/// Users and roles that can receive grants.
fn grantees(conn: &Connection) -> Result<Vec<String>, DbError> {
    let mut out = names(
        conn,
        "select username from all_users order by username",
        &[],
    )?;
    // DBA_ROLES needs catalog access; the session's roles are the fallback.
    let roles = names(conn, "select role from dba_roles order by role", &[])
        .or_else(|_| names(conn, "select role from session_roles order by role", &[]))?;
    out.extend(roles);
    out.push("PUBLIC".into());
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn names(
    conn: &Connection,
    sql: &str,
    binds: &[&dyn pythia_db::oracle::sql_type::ToSql],
) -> Result<Vec<String>, DbError> {
    let mut out = Vec::new();
    for row in conn.query_as::<String>(sql, binds)? {
        out.push(row?);
    }
    Ok(out)
}

/// Runs the statements in order, stopping at the first error. A view's
/// compile errors (with `FORCE`) come back as warnings.
fn apply(
    conn: &Connection,
    statements: &[String],
    view: Option<(String, String)>,
) -> Result<Vec<String>, ApplyError> {
    let mut dropped = false;
    for s in statements {
        conn.execute(s, &[]).map_err(|e| ApplyError {
            statement: s.clone(),
            error: e.into(),
            dropped,
        })?;
        dropped |= s.trim_start().to_ascii_lowercase().starts_with("drop ");
    }
    let mut warnings = Vec::new();
    if let Some((owner, name)) = view
        && let Ok(rows) = conn.query_as::<String>(
            "select text from all_errors where owner = :1 and name = :2 and type = 'VIEW'
              order by sequence",
            &[&owner, &name],
        )
    {
        for text in rows.flatten() {
            warnings.push(format!("Created with errors: {}", text.trim()));
        }
    }
    Ok(warnings)
}

pub fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

/// The statements as a runnable script.
/// The statements as a script: PL/SQL blocks end with `/`.
fn script(statements: &[String]) -> String {
    statements
        .iter()
        .map(|s| {
            let lower = s.trim_start().to_lowercase();
            if lower.starts_with("begin") || lower.starts_with("declare") {
                format!("{s}\n/")
            } else {
                format!("{s};")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The dictionary form of a name as typed: quoted names verbatim, others
/// upper case.
pub fn ident(input: &str) -> String {
    let s = input.trim();
    match s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(quoted) => quoted.replace("\"\"", "\""),
        None => s.to_uppercase(),
    }
}

/// `owner.name` with `name` as typed.
pub fn qualified(owner: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(owner), quote_ident(&ident(name)))
}

pub fn is_integer(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

pub fn form_grid(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([16.0, 8.0])
        .min_col_width(110.0)
        .show(ui, add);
}

pub fn form_row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(label);
    ui.horizontal(add);
    ui.end_row();
}

/// The name: editable for new objects, fixed for existing ones.
pub fn name_field(ui: &mut Ui, name: &mut String, existing: bool) {
    form_row(ui, "Name", |ui| {
        if existing {
            ui.label(RichText::new(name.as_str()).monospace().strong());
        } else {
            ui.add(
                egui::TextEdit::singleline(name)
                    .hint_text("name")
                    .desired_width(240.0),
            );
        }
    });
}

/// A "▾" button listing `items` with a filter; returns the one picked.
pub fn picker(ui: &mut Ui, id: impl Hash + std::fmt::Debug, items: &[String]) -> Option<String> {
    let filter_id = ui.id().with(("picker-filter", &id));
    let mut picked = None;
    ui.menu_button("▾", |ui| {
        let mut filter: String = ui.data(|d| d.get_temp(filter_id)).unwrap_or_default();
        let response = ui.add(
            egui::TextEdit::singleline(&mut filter)
                .hint_text("Filter…")
                .desired_width(220.0),
        );
        if !response.has_focus() && filter.is_empty() {
            response.request_focus();
        }
        let needle = filter.to_uppercase();
        egui::ScrollArea::vertical()
            .max_height(280.0)
            .show(ui, |ui| {
                let mut shown = 0;
                for item in items.iter().filter(|i| i.to_uppercase().contains(&needle)) {
                    if shown == 300 {
                        ui.weak("… type to narrow down");
                        break;
                    }
                    shown += 1;
                    if ui.button(RichText::new(item).monospace()).clicked() {
                        picked = Some(item.clone());
                        ui.close();
                    }
                }
                if shown == 0 {
                    ui.weak("Nothing found");
                }
            });
        ui.data_mut(|d| d.insert_temp(filter_id, filter));
    });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_names() {
        assert_eq!(ident(" orders "), "ORDERS");
        assert_eq!(ident("\"Mixed\""), "Mixed");
        assert_eq!(qualified("HR", "emp"), "HR.EMP");
        assert_eq!(qualified("HR", "\"my emp\""), "HR.\"my emp\"");
        assert!(is_integer("-12") && !is_integer("1.5") && !is_integer("-"));
    }

    #[test]
    fn scripts_end_blocks_with_a_slash() {
        let statements = [
            "create sequence s".to_owned(),
            "begin\n  dbms_scheduler.enable('J');\nend;".to_owned(),
        ];
        assert_eq!(
            script(&statements),
            "create sequence s;\n\nbegin\n  dbms_scheduler.enable('J');\nend;\n/"
        );
    }

    /// A scheduler job through the job form's SQL: created, read back,
    /// its next runs, changed and dropped.
    #[test]
    fn builds_live_jobs() {
        let Some((conn, _)) = connect("PYTHIA_TEST_DB") else {
            return;
        };
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let drop = crate::jobs_window::action_sql("drop", &owner, "PYOB_JOB");
        let _ = conn.execute(&drop, &[]);
        conn.execute(
            "create or replace procedure pyob_job_p is begin null; end;",
            &[],
        )
        .unwrap();
        let run = |statements: Vec<String>| {
            apply(&conn, &statements, None).unwrap_or_else(|e| {
                panic!("{}: {}", e.statement, e.error);
            })
        };
        let mut base = BaseLists::default();
        job::lists(&conn, &owner, &mut base).unwrap();
        assert!(base.procedures.iter().any(|p| p == "pyob_job_p"));
        assert_eq!(base.session_user, owner);
        // 2030-01-01 is a Tuesday.
        let start = "2030-01-01 02:00";
        let repeat = "FREQ=WEEKLY;BYDAY=MON,FRI;BYHOUR=7;BYMINUTE=30;BYSECOND=0";
        let runs = job::next_runs(&conn, start, repeat).unwrap();
        assert_eq!(
            runs,
            [
                "Fri 2030-01-04 07:30",
                "Mon 2030-01-07 07:30",
                "Fri 2030-01-11 07:30",
                "Mon 2030-01-14 07:30",
                "Fri 2030-01-18 07:30",
            ]
        );
        assert!(job::next_runs(&conn, "", "FREQ=SOMETIMES").is_err());
        if !base.session_privileges.iter().any(|p| p == "CREATE JOB") {
            eprintln!("{owner} can't create jobs, skipping");
            conn.execute("drop procedure pyob_job_p", &[]).unwrap();
            return;
        }

        let def = job::JobDef {
            name: "pyob_job".into(),
            kind: job::JobKind::Procedure,
            action: "pyob_job_p".into(),
            start: start.into(),
            repeat: repeat.into(),
            enabled: false,
            comments: "Pythia's test job".into(),
            ..Default::default()
        };
        run(job::statements(&owner, None, &def).unwrap());
        let loaded = job::load(&conn, &owner, "PYOB_JOB").unwrap();
        assert_eq!(loaded.kind, job::JobKind::Procedure);
        assert_eq!(loaded.action, "PYOB_JOB_P");
        assert_eq!(loaded.start, def.start);
        assert_eq!(loaded.repeat, def.repeat);
        assert!(!loaded.enabled && !loaded.auto_drop);
        assert_eq!(loaded.comments, def.comments);
        assert_eq!(loaded.fixed_action, None);
        assert_eq!(loaded.fixed_schedule, None);
        assert_eq!(
            job::statements(&owner, Some(&loaded), &loaded),
            Ok(Vec::new())
        );

        let mut edited = loaded.clone();
        edited.kind = job::JobKind::Block;
        edited.action = "begin pyob_job_p; end;".into();
        edited.repeat = "FREQ=HOURLY;INTERVAL=4".into();
        edited.end = "2031-01-01 00:00".into();
        edited.comments = String::new();
        edited.enabled = true;
        run(job::statements(&owner, Some(&loaded), &edited).unwrap());
        let reloaded = job::load(&conn, &owner, "PYOB_JOB").unwrap();
        assert_eq!(reloaded.kind, job::JobKind::Block);
        assert_eq!(reloaded.action, edited.action);
        assert_eq!(reloaded.repeat, edited.repeat);
        assert_eq!(reloaded.end, edited.end);
        assert!(reloaded.enabled);
        assert_eq!(reloaded.comments, "");
        assert_eq!(
            job::statements(&owner, Some(&reloaded), &reloaded),
            Ok(Vec::new())
        );
        conn.execute(&drop, &[]).unwrap();
        conn.execute("drop procedure pyob_job_p", &[]).unwrap();
    }

    /// Creates, alters and drops real objects through the editors' SQL.
    #[test]
    fn edits_live_objects() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        // Its synonyms and grants deadlock with the account tests' DDL too
        // (#126).
        let _accounts = crate::test_db::ACCOUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        conn.execute(
            "begin
               for o in (select object_name, object_type from user_objects
                          where object_name like 'PYTHIA\\_OE\\_%' escape '\\'
                            and object_type in ('TABLE', 'VIEW', 'SEQUENCE', 'SYNONYM')) loop
                 execute immediate 'drop ' || o.object_type || ' ' || o.object_name
                   || case o.object_type when 'TABLE' then ' purge' end;
               end loop;
             end;",
            &[],
        )
        .unwrap();
        let run = |statements: Vec<String>| {
            apply(&conn, &statements, None).unwrap_or_else(|e| {
                panic!("{}: {}", e.statement, e.error);
            })
        };
        conn.execute(
            "create table pythia_oe_t (id number primary key, name varchar2(30), created date)",
            &[],
        )
        .unwrap();

        // Sequence: create, reload, alter.
        let mut seq = sequence::SequenceDef::new();
        seq.name = "pythia_oe_seq".into();
        seq.start = "100".into();
        run(sequence::statements(&owner, None, &seq).unwrap());
        let loaded = sequence::load(&conn, &owner, "PYTHIA_OE_SEQ").unwrap();
        assert_eq!(loaded.increment, "1");
        assert_eq!(loaded.min, "1");
        assert_eq!(loaded.max, "");
        assert_eq!(loaded.cache, 20);
        let mut edited = loaded.clone();
        edited.increment = "5".into();
        edited.cache = 0;
        // Restarting needs 18c; before, the sequence just goes on.
        if loaded.can_restart {
            edited.start = "500".into();
        }
        run(sequence::statements(&owner, Some(&loaded), &edited).unwrap());
        let next: i64 = conn
            .query_row_as("select pythia_oe_seq.nextval from dual", &[])
            .unwrap();
        // Without a restart it goes on; a new increment before the first
        // NEXTVAL shifts the first value, so only the restart is exact.
        if loaded.can_restart {
            assert_eq!(next, 500);
        } else {
            assert!(next >= 100, "{next}");
        }
        assert_eq!(
            sequence::load(&conn, &owner, "PYTHIA_OE_SEQ")
                .unwrap()
                .cache,
            0
        );

        // View with check option, reloaded without the clause in the query.
        let mut v = view::ViewDef::new();
        v.name = "pythia_oe_v".into();
        v.query = "select id, name from pythia_oe_t where id > 0".into();
        v.check_option = true;
        run(view::statements(&owner, &v).unwrap());
        let loaded = view::load(&conn, &owner, "PYTHIA_OE_V").unwrap();
        assert!(loaded.check_option && !loaded.read_only);
        assert_eq!(
            loaded.query,
            "select id, name from pythia_oe_t where id > 0"
        );
        // A broken query with FORCE: created with errors.
        let mut broken = loaded.clone();
        broken.query = "select nope from pythia_oe_t".into();
        broken.force = true;
        broken.check_option = false;
        let warnings = apply(
            &conn,
            &view::statements(&owner, &broken).unwrap(),
            Some((owner.clone(), "PYTHIA_OE_V".into())),
        )
        .unwrap();
        assert!(!warnings.is_empty(), "expected compile errors");

        // Synonym.
        let syn = synonym::SynonymDef {
            name: "pythia_oe_syn".into(),
            target_name: "pythia_oe_t".into(),
            ..Default::default()
        };
        run(synonym::statements(&owner, &syn).unwrap());
        let loaded = synonym::load(&conn, &owner, "PYTHIA_OE_SYN").unwrap();
        assert_eq!(loaded.target_name, "PYTHIA_OE_T");
        assert_eq!(loaded.target_owner, quote_ident(&owner));

        // Index: create, then recreate with another column order.
        let mut ix = index::IndexDef::new();
        ix.name = "pythia_oe_ix".into();
        ix.table = "pythia_oe_t".into();
        ix.columns = vec![
            index::IndexColumn {
                expr: "name".into(),
                desc: false,
            },
            index::IndexColumn {
                expr: "created".into(),
                desc: true,
            },
        ];
        run(index::statements(&owner, None, &ix).unwrap());
        let loaded = index::load(&conn, &owner, "PYTHIA_OE_IX").unwrap();
        assert_eq!(loaded.columns.len(), 2);
        assert_eq!(loaded.columns[0].expr, "NAME");
        assert_eq!(loaded.columns[1].expr, "CREATED");
        assert!(loaded.columns[1].desc);
        let mut edited = loaded.clone();
        edited.columns.swap(0, 1);
        edited.unique = true;
        run(index::statements(&owner, Some(&loaded), &edited).unwrap());
        let reloaded = index::load(&conn, &owner, "PYTHIA_OE_IX").unwrap();
        assert!(reloaded.unique);
        assert_eq!(reloaded.columns[0].expr, "CREATED");
        // Made unique on a column with duplicates: the drop runs, the
        // create fails, and the error says the index is gone (the form
        // then only creates it).
        conn.execute(
            "insert into pythia_oe_t values (1, 'twin', sysdate - 1)",
            &[],
        )
        .unwrap();
        conn.execute("insert into pythia_oe_t values (2, 'twin', sysdate)", &[])
            .unwrap();
        let mut twins = reloaded.clone();
        twins.columns.clear();
        twins.columns.push(index::IndexColumn {
            expr: "name".into(),
            desc: false,
        });
        let e = apply(
            &conn,
            &index::statements(&owner, Some(&reloaded), &twins).unwrap(),
            None,
        )
        .unwrap_err();
        assert!(e.dropped && e.error.code == Some(1452), "{e:?}");
        twins.unique = false;
        run(index::statements(&owner, None, &twins).unwrap());
        assert!(!index::load(&conn, &owner, "PYTHIA_OE_IX").unwrap().unique);

        // Grants to PUBLIC: add with grant option is refused for PUBLIC, so
        // grant plain select and revoke it again.
        let object = format!("{}.PYTHIA_OE_T", quote_ident(&owner));
        let before = grants::load(&conn, &owner, "PYTHIA_OE_T", ObjectType::Table).unwrap();
        let mut after = before.clone();
        after.rows.push(grants::GrantRow {
            grantee: "PUBLIC".into(),
            granted: ["SELECT".to_owned()].into(),
            grantable: BTreeSet::new(),
        });
        run(grants::statements(&object, &before, &after));
        let loaded = grants::load(&conn, &owner, "PYTHIA_OE_T", ObjectType::Table).unwrap();
        assert!(
            loaded
                .rows
                .iter()
                .any(|r| r.grantee == "PUBLIC" && r.granted.contains("SELECT"))
        );
        run(grants::statements(&object, &loaded, &before));
        let loaded = grants::load(&conn, &owner, "PYTHIA_OE_T", ObjectType::Table).unwrap();
        assert!(!loaded.rows.iter().any(|r| r.grantee == "PUBLIC"));
        // A DBA sees grants it takes no part in (#47).
        if let Some((dba, _)) = connect("PYTHIA_TEST_DB_DBA") {
            conn.execute("grant select on pythia_oe_t to resource", &[])
                .unwrap();
            let seen = grants::load(&dba, &owner, "PYTHIA_OE_T", ObjectType::Table).unwrap();
            conn.execute("revoke select on pythia_oe_t from resource", &[])
                .unwrap();
            assert!(seen.rows.iter().any(|r| r.grantee == "RESOURCE"));
        }

        for s in [
            "drop synonym pythia_oe_syn",
            "drop view pythia_oe_v",
            "drop sequence pythia_oe_seq",
            "drop table pythia_oe_t purge",
        ] {
            conn.execute(s, &[]).unwrap();
        }
    }
    fn connect(var: &str) -> Option<(Connection, String)> {
        let spec = std::env::var(var).ok()?;
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        Some((Connection::connect(user, pw, db).unwrap(), spec))
    }

    /// Compiles a unit's source as a program window does; its errors.
    fn compile(conn: &Connection, owner: &str, source: &str, name: &str) -> Vec<String> {
        conn.execute(source.trim_end().trim_end_matches('/').trim_end(), &[])
            .unwrap_or_else(|e| panic!("{e}\n{source}"));
        names(
            conn,
            "select line || ': ' || text from all_errors where owner = :1 and name = :2
              order by type, sequence",
            &[&owner, &name],
        )
        .unwrap()
    }

    /// The new forms' SQL against the database: materialized view and the
    /// first sources of a trigger, procedure, function, package
    /// and types, which must compile as they are.
    #[test]
    fn builds_live_objects() {
        let Some((conn, _)) = connect("PYTHIA_TEST_DB") else {
            return;
        };
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let cleanup = "begin
               for o in (select object_name, object_type from user_objects
                          where object_name like 'PYTHIA\\_OB\\_%' escape '\\'
                            and object_type in ('TABLE', 'MATERIALIZED VIEW', 'PROCEDURE',
                                'FUNCTION', 'PACKAGE', 'TYPE', 'VIEW')
                          order by decode(object_type, 'MATERIALIZED VIEW', 0, 'TYPE', 2, 1),
                                   object_name desc) loop
                 -- A materialized view's table goes with it.
                 begin
                   execute immediate 'drop ' || o.object_type || ' ' || o.object_name
                     || case o.object_type when 'TABLE' then ' cascade constraints purge'
                                           when 'TYPE' then ' force' end;
                 exception when others then null;
                 end;
               end loop;
             end;";
        conn.execute(cleanup, &[]).unwrap();
        let run = |statements: Vec<String>| {
            apply(&conn, &statements, None).unwrap_or_else(|e| {
                panic!("{}: {}", e.statement, e.error);
            })
        };
        conn.execute(
            "create table pythia_ob_t (id number primary key, status varchar2(10), total number)",
            &[],
        )
        .unwrap();
        conn.execute(
            "create view pythia_ob_v as select id, status from pythia_ob_t",
            &[],
        )
        .unwrap();

        // Materialized view: create, reload, change refresh and rewrite.
        let mut mv = mview::MviewDef::new();
        mv.name = "pythia_ob_mv".into();
        mv.query = "select status, count(*) n from pythia_ob_t group by status".into();
        mv.refresh = mview::Refresh::Complete;
        run(mview::statements(&owner, None, &mv).unwrap());
        let loaded = mview::load(&conn, &owner, "PYTHIA_OB_MV").unwrap();
        assert_eq!(loaded.refresh, mview::Refresh::Complete);
        assert!(!loaded.on_commit && !loaded.rewrite);
        assert_eq!(loaded.query.to_lowercase(), mv.query);
        let mut edited = loaded.clone();
        edited.refresh = mview::Refresh::Force;
        edited.next = "sysdate + 1".into();
        run(mview::statements(&owner, Some(&loaded), &edited).unwrap());
        assert_eq!(
            mview::load(&conn, &owner, "PYTHIA_OB_MV").unwrap().refresh,
            mview::Refresh::Force
        );
        let mut requery = edited.clone();
        requery.query = "select status from pythia_ob_t".into();
        run(mview::statements(&owner, Some(&edited), &requery).unwrap());
        assert_eq!(
            mview::load(&conn, &owner, "PYTHIA_OB_MV")
                .unwrap()
                .query
                .to_lowercase(),
            "select status from pythia_ob_t"
        );
        // A mistake in a new query fails before the drop: the view stays.
        let mut broken = requery.clone();
        broken.query = "select nope from pythia_ob_t".into();
        let e = apply(
            &conn,
            &mview::statements(&owner, Some(&requery), &broken).unwrap(),
            None,
        )
        .unwrap_err();
        assert!(
            !e.dropped && e.statement.contains("where 1 = 0"),
            "{}",
            e.statement
        );
        assert!(mview::load(&conn, &owner, "PYTHIA_OB_MV").is_ok());

        // Triggers: on the table (row, statement, compound) and the view.
        let mut t = trigger::TriggerDef::on("PYTHIA_OB_T", false);
        t.update_columns = "status".into();
        t.when = "new.status is not null".into();
        let src = trigger::source(&owner, &t).unwrap();
        assert_eq!(
            compile(&conn, &owner, &src, "PYTHIA_OB_T_BIU"),
            Vec::<String>::new()
        );
        t.compound = true;
        t.when.clear();
        t.delete = true;
        let src = trigger::source(&owner, &t).unwrap();
        assert_eq!(
            compile(&conn, &owner, &src, "PYTHIA_OB_T_CIUD"),
            Vec::<String>::new()
        );
        t.compound = false;
        t.row_level = false;
        t.timing = trigger::Timing::After;
        t.enabled = false;
        let src = trigger::source(&owner, &t).unwrap();
        assert_eq!(
            compile(&conn, &owner, &src, "PYTHIA_OB_T_AIUD"),
            Vec::<String>::new()
        );
        let status: String = conn
            .query_row_as(
                "select status from user_triggers where trigger_name = 'PYTHIA_OB_T_AIUD'",
                &[],
            )
            .unwrap();
        assert_eq!(status, "DISABLED");
        let v = trigger::TriggerDef::on("PYTHIA_OB_V", true);
        let src = trigger::source(&owner, &v).unwrap();
        assert_eq!(
            compile(&conn, &owner, &src, "PYTHIA_OB_V_IOIU"),
            Vec::<String>::new()
        );

        // Procedure, function and package (spec and body with stubs).
        let mut p = program::ProgramDef::new(ObjectType::Procedure);
        p.name = "pythia_ob_proc".into();
        p.subprograms[0].params = vec![
            program::Param {
                name: "p_id".into(),
                mode: program::Mode::In,
                data_type: "pythia_ob_t.id%type".into(),
                default: String::new(),
            },
            program::Param {
                name: "p_out".into(),
                mode: program::Mode::Out,
                data_type: "varchar2".into(),
                default: String::new(),
            },
        ];
        p.invoker_rights = true;
        let src = program::sources(&owner, &p).unwrap();
        assert_eq!(
            compile(&conn, &owner, &src[0], "PYTHIA_OB_PROC"),
            Vec::<String>::new()
        );
        let mut f = program::ProgramDef::new(ObjectType::Function);
        f.name = "pythia_ob_func".into();
        let src = program::sources(&owner, &f).unwrap();
        assert_eq!(
            compile(&conn, &owner, &src[0], "PYTHIA_OB_FUNC"),
            Vec::<String>::new()
        );
        let mut pkg = program::ProgramDef::new(ObjectType::Package);
        pkg.name = "pythia_ob_pkg".into();
        pkg.subprograms = vec![
            program::Subprogram {
                function: false,
                name: "start_job".into(),
                params: p.subprograms[0].params.clone(),
                returns: String::new(),
            },
            program::Subprogram {
                function: true,
                name: "status_of".into(),
                params: Vec::new(),
                returns: "varchar2".into(),
            },
        ];
        let src = program::sources(&owner, &pkg).unwrap();
        assert_eq!(
            compile(&conn, &owner, &src[0], "PYTHIA_OB_PKG"),
            Vec::<String>::new()
        );
        assert_eq!(
            compile(&conn, &owner, &src[1], "PYTHIA_OB_PKG"),
            Vec::<String>::new()
        );

        // Types: an object type and collections of it.
        let mut ty = type_def::TypeDef::new();
        ty.name = "pythia_ob_obj_t".into();
        ty.attributes.push(type_def::Attribute {
            name: "label".into(),
            data_type: "varchar2(100)".into(),
        });
        ty.not_final = true;
        let src = type_def::sources(&owner, &ty).unwrap().remove(0);
        assert_eq!(
            compile(&conn, &owner, &src, "PYTHIA_OB_OBJ_T"),
            Vec::<String>::new()
        );
        let list = type_def::TypeDef {
            name: "pythia_ob_list_t".into(),
            kind: type_def::TypeKind::Table,
            element: "pythia_ob_obj_t".into(),
            ..type_def::TypeDef::new()
        };
        let src = type_def::sources(&owner, &list).unwrap().remove(0);
        assert_eq!(
            compile(&conn, &owner, &src, "PYTHIA_OB_LIST_T"),
            Vec::<String>::new()
        );
        let array = type_def::TypeDef {
            name: "pythia_ob_array_t".into(),
            kind: type_def::TypeKind::Varray,
            element: "number".into(),
            limit: 5,
            ..type_def::TypeDef::new()
        };
        let src = type_def::sources(&owner, &array).unwrap().remove(0);
        assert_eq!(
            compile(&conn, &owner, &src, "PYTHIA_OB_ARRAY_T"),
            Vec::<String>::new()
        );

        // An object type with methods: spec and body compile as they are.
        let mut with_methods = type_def::TypeDef::new();
        with_methods.name = "pythia_ob_meth_t".into();
        with_methods.not_final = true;
        with_methods.methods = vec![
            type_def::Method {
                function: true,
                name: "label".into(),
                returns: "varchar2".into(),
                ..Default::default()
            },
            type_def::Method {
                kind: type_def::MethodKind::Static,
                name: "reset".into(),
                ..Default::default()
            },
            type_def::Method {
                kind: type_def::MethodKind::Constructor,
                params: vec![program::Param {
                    name: "p_id".into(),
                    mode: program::Mode::In,
                    data_type: "number".into(),
                    default: String::new(),
                }],
                ..Default::default()
            },
            type_def::Method {
                kind: type_def::MethodKind::Map,
                name: "sort_key".into(),
                returns: "number".into(),
                ..Default::default()
            },
        ];
        let src = type_def::sources(&owner, &with_methods).unwrap();
        assert_eq!(src.len(), 2);
        assert_eq!(
            compile(&conn, &owner, &src[0], "PYTHIA_OB_METH_T"),
            Vec::<String>::new()
        );
        assert_eq!(
            compile(&conn, &owner, &src[1], "PYTHIA_OB_METH_T"),
            Vec::<String>::new()
        );

        let invalid: Vec<String> = names(
            &conn,
            "select object_type || ' ' || object_name from user_objects
              where object_name like 'PYTHIA\\_OB\\_%' escape '\\' and status <> 'VALID'",
            &[],
        )
        .unwrap();
        assert!(invalid.is_empty(), "{invalid:?}");
        conn.execute(cleanup, &[]).unwrap();
    }

    /// A database link back to the test database (needs CREATE DATABASE
    /// LINK, so it runs as the DBA).
    #[test]
    fn creates_live_link() {
        let Some((conn, spec)) = connect("PYTHIA_TEST_DB_DBA") else {
            return;
        };
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        conn.execute(
            "begin execute immediate 'drop database link pythia_ob_link';
             exception when others then null; end;",
            &[],
        )
        .unwrap();
        let run = |statements: Vec<String>| {
            apply(&conn, &statements, None).unwrap_or_else(|e| {
                panic!("{}: {}", e.statement, e.error);
            })
        };
        let (cred, _) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        // The server resolves the link: its own service on the default port
        // (the test's host and port may be a mapping into a container).
        let service: String = conn
            .query_row_as(
                "select sys_context('USERENV', 'SERVICE_NAME') from dual",
                &[],
            )
            .unwrap();
        let db = format!("localhost/{service}");
        let link = db_link::LinkDef {
            public: false,
            name: "pythia_ob_link".into(),
            user: user.into(),
            password: pw.into(),
            using: db.clone(),
        };
        run(db_link::statements(None, &link).unwrap());
        let loaded = db_link::load(&conn, &owner, "PYTHIA_OB_LINK").unwrap();
        assert_eq!(loaded.user, user.to_lowercase());
        assert_eq!(loaded.using, db);
        let mut edited = loaded.clone();
        edited.password = pw.into();
        let s = db_link::statements(Some(&loaded), &edited).unwrap();
        assert!(s[0].starts_with("drop database link"), "{s:?}");
        run(s);
        // On a session of its own: one that used the link can't drop it.
        let (other, _) = connect("PYTHIA_TEST_DB_DBA").unwrap();
        let one: i64 = other
            .query_row_as("select 1 from dual@pythia_ob_link", &[])
            .unwrap();
        assert_eq!(one, 1);
        drop(other);
        conn.execute("drop database link pythia_ob_link", &[])
            .unwrap();
    }

    /// A link's remote user reads as SQL writes it: a quoted "r83_lower"
    /// stays the user it is when the link is made again (#365).
    #[test]
    fn links_keep_their_quoted_user() {
        let Some((conn, _)) = connect("PYTHIA_TEST_DB_DBA") else {
            return;
        };
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let _ = conn.execute("drop database link r83_link", &[]);
        conn.execute(
            "create database link r83_link connect to \"r83_lower\" identified by \"x\" \
             using 'nowhere'",
            &[],
        )
        .unwrap();
        let loaded = db_link::load(&conn, &owner, "R83_LINK");
        conn.execute("drop database link r83_link", &[]).unwrap();
        let loaded = loaded.unwrap();
        assert_eq!(loaded.user, "\"r83_lower\"");
        let mut edited = loaded.clone();
        edited.password = "y".into();
        let s = db_link::statements(Some(&loaded), &edited).unwrap();
        assert!(
            s[1].contains(" connect to \"r83_lower\" identified by"),
            "{s:?}"
        );
    }

    /// Users and roles through the account form's SQL (needs a DBA).
    #[test]
    fn creates_live_accounts() {
        let Some((conn, _)) = connect("PYTHIA_TEST_DB_DBA") else {
            return;
        };
        let _accounts = crate::test_db::ACCOUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cleanup = "begin
               for u in (select username from dba_users where username = 'PYTHIA_OB_USER') loop
                 execute immediate 'drop user pythia_ob_user cascade';
               end loop;
               for r in (select role from dba_roles where role = 'PYTHIA_OB_ROLE') loop
                 execute immediate 'drop role pythia_ob_role';
               end loop;
               for t in (select table_name from dba_tables
                          where owner = 'SYSTEM' and table_name = 'PYTHIA_OB_ACC_T') loop
                 execute immediate 'drop table system.pythia_ob_acc_t purge';
               end loop;
             end;";
        conn.execute(cleanup, &[]).unwrap();
        conn.execute("create table system.pythia_ob_acc_t (id number)", &[])
            .unwrap();
        let run = |statements: Vec<String>| {
            apply(&conn, &statements, None).unwrap_or_else(|e| {
                panic!("{}: {}", e.statement, e.error);
            })
        };
        let mut base = BaseLists::default();
        account::lists(&conn, &mut base).unwrap();
        assert!(base.privileges.iter().any(|p| p == "CREATE TABLE"));
        assert!(base.roles.iter().any(|r| r == "CONNECT"));
        assert!(base.owners.iter().any(|o| o == "SYSTEM"));
        assert!(base.session_privileges.iter().any(|p| p == "CREATE USER"));
        let tablespace = base
            .tablespaces
            .iter()
            .find(|t| *t == "USERS")
            .cloned()
            .unwrap_or_else(|| base.tablespaces[0].clone());
        let other = base
            .tablespaces
            .iter()
            .find(|t| **t != tablespace && *t != "SYSTEM")
            .cloned()
            .unwrap_or_else(|| "SYSTEM".into());
        let table = ("SYSTEM".to_owned(), "PYTHIA_OB_ACC_T".to_owned());
        let object = |p: &str| (table.0.clone(), table.1.clone(), p.to_owned());

        let mut role = account::AccountDef::new_role();
        role.name = "pythia_ob_role".into();
        role.privileges.insert("CREATE SESSION".into(), false);
        role.objects.insert(object("SELECT"), false);
        run(account::statements(None, &role).unwrap());
        let loaded = account::load_role(&conn, "PYTHIA_OB_ROLE").unwrap();
        assert_eq!(loaded.auth, account::Auth::None);
        assert_eq!(loaded.privileges, role.privileges);
        assert_eq!(loaded.objects, role.objects);

        let mut user = account::AccountDef::new_user();
        user.name = "pythia_ob_user".into();
        user.password = "Pythia_ob_1".into();
        user.default_tablespace = tablespace.clone();
        user.quotas = vec![
            (tablespace.clone(), "10M".into()),
            (other.clone(), "unlimited".into()),
        ];
        user.roles.insert(
            "PYTHIA_OB_ROLE".into(),
            account::RoleGrant {
                admin: true,
                default: true,
            },
        );
        user.roles.insert(
            "CONNECT".into(),
            account::RoleGrant {
                admin: false,
                default: false,
            },
        );
        user.privileges.insert("CREATE TABLE".into(), true);
        user.objects.insert(object("UPDATE"), true);
        run(account::statements(None, &user).unwrap());
        let loaded = account::load_user(&conn, "PYTHIA_OB_USER").unwrap();
        assert_eq!(loaded.auth, account::Auth::Password);
        assert_eq!(ident(&loaded.default_tablespace), tablespace);
        let mut quotas: Vec<(String, String)> = loaded
            .quotas
            .iter()
            .map(|(t, s)| (ident(t), s.clone()))
            .collect();
        quotas.sort();
        let mut expected = vec![
            (tablespace.clone(), "10M".to_owned()),
            (other.clone(), "unlimited".to_owned()),
        ];
        expected.sort();
        assert_eq!(quotas, expected);
        assert_eq!(loaded.roles, user.roles, "admin option and default roles");
        assert_eq!(loaded.privileges, user.privileges);
        assert_eq!(loaded.objects, user.objects);
        assert!(loaded.locked == Some(false) && !loaded.expire);
        // Nothing to do on the loaded user.
        assert_eq!(account::statements(Some(&loaded), &loaded), Ok(Vec::new()));

        let mut edited = loaded.clone();
        edited.locked = Some(true);
        edited.quotas.retain(|(t, _)| ident(t) == tablespace);
        edited.quotas[0].1 = "unlimited".into();
        edited.privileges.insert("CREATE TABLE".into(), false);
        edited.roles.get_mut("CONNECT").unwrap().default = true;
        edited.roles.get_mut("PYTHIA_OB_ROLE").unwrap().admin = false;
        edited.objects.insert(object("UPDATE"), false);
        edited.objects.insert(object("SELECT"), false);
        run(account::statements(Some(&loaded), &edited).unwrap());
        let reloaded = account::load_user(&conn, "PYTHIA_OB_USER").unwrap();
        assert_eq!(reloaded.locked, Some(true));
        assert_eq!(reloaded.quotas.len(), 1);
        assert_eq!(reloaded.quotas[0].1, "unlimited");
        assert_eq!(reloaded.privileges, edited.privileges);
        assert_eq!(reloaded.roles, edited.roles);
        assert_eq!(reloaded.objects, edited.objects);
        assert_eq!(
            account::statements(Some(&reloaded), &reloaded),
            Ok(Vec::new())
        );
        conn.execute(cleanup, &[]).unwrap();
    }

    /// Another user and a role, read and changed by an account with ALTER
    /// USER and the GRANT ANY privileges but without SELECT_CATALOG_ROLE:
    /// first with AUDIT_ADMIN, which reads DBA_USERS (not the grants), then
    /// without any DBA view (#89).
    #[test]
    fn edits_accounts_without_dba_views() {
        let Some((dba, spec)) = connect("PYTHIA_TEST_DB_DBA") else {
            return;
        };
        let _accounts = crate::test_db::ACCOUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let db = spec.rsplit_once('@').unwrap().1.to_owned();
        let cleanup = "begin
               for u in (select username from dba_users
                          where username in ('PYTHIA_OB_LIMITED', 'PYTHIA_OB_TARGET')) loop
                 execute immediate 'drop user ' || u.username || ' cascade';
               end loop;
               for r in (select role from dba_roles where role = 'PYTHIA_OB_TROLE') loop
                 execute immediate 'drop role pythia_ob_trole';
               end loop;
             end;";
        dba.execute(cleanup, &[]).unwrap();
        for sql in [
            "create user pythia_ob_limited identified by \"Pythia_ob_2\"",
            "grant create session, alter user, grant any role, grant any privilege \
             to pythia_ob_limited",
            "grant audit_admin to pythia_ob_limited",
            "create role pythia_ob_trole",
            "grant create view to pythia_ob_trole",
            "create user pythia_ob_target identified by \"Pythia_ob_3\"",
            "grant create session, create table to pythia_ob_target",
            "grant pythia_ob_trole to pythia_ob_target",
        ] {
            dba.execute(sql, &[])
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let limited = Connection::connect("pythia_ob_limited", "Pythia_ob_2", &db).unwrap();

        // DBA_USERS (AUDIT_ADMIN): the attributes, but not the grants.
        let loaded = account::load_user(&limited, "PYTHIA_OB_TARGET").unwrap();
        assert!(!loaded.unread.attributes);
        assert_eq!(loaded.locked, Some(false));
        assert!(loaded.unread.roles && loaded.unread.privileges && loaded.unread.quotas);
        assert!(loaded.unread.objects);
        assert!(loaded.roles.is_empty() && loaded.privileges.is_empty());
        assert_eq!(account::statements(Some(&loaded), &loaded), Ok(Vec::new()));

        // No DBA view at all: the user is still found, and the form works.
        dba.execute("revoke audit_admin from pythia_ob_limited", &[])
            .unwrap();
        drop(limited);
        let limited = Connection::connect("pythia_ob_limited", "Pythia_ob_2", &db).unwrap();
        let loaded = account::load_user(&limited, "PYTHIA_OB_TARGET").unwrap();
        assert!(loaded.unread.attributes && loaded.unread.roles);
        assert_eq!(loaded.locked, None);
        let missing = account::load_user(&limited, "PYTHIA_OB_NOBODY").unwrap_err();
        assert!(missing.message.contains("not found"), "{}", missing.message);
        let mut edited = loaded.clone();
        edited.locked = Some(true);
        edited.privileges.insert("CREATE VIEW".into(), false);
        edited
            .roles
            .insert("CONNECT".into(), account::RoleGrant::default());
        let statements = account::statements(Some(&loaded), &edited).unwrap();
        assert!(
            statements
                .iter()
                .all(|s| !s.starts_with("revoke") && !s.contains("default role")),
            "{statements:?}"
        );
        apply(&limited, &statements, None).unwrap_or_else(|e| {
            panic!("{}: {}", e.statement, e.error);
        });
        // As the DBA sees it: changed, and nothing it had was taken away.
        let now = account::load_user(&dba, "PYTHIA_OB_TARGET").unwrap();
        assert_eq!(now.locked, Some(true));
        for p in ["CREATE SESSION", "CREATE TABLE", "CREATE VIEW"] {
            assert!(now.privileges.contains_key(p), "{p}: {:?}", now.privileges);
        }
        assert!(now.roles.contains_key("PYTHIA_OB_TROLE") && now.roles.contains_key("CONNECT"));

        // A role not enabled in the session: its grants are unknown, not none.
        let role = account::load_role(&limited, "PYTHIA_OB_TROLE").unwrap();
        assert!(role.unread.privileges && role.unread.roles);
        assert!(role.privileges.is_empty());
        drop(limited);
        dba.execute(cleanup, &[]).unwrap();
    }

    /// Directories through the directory form's SQL, and grants on them
    /// through the grants editor's (needs a DBA).
    #[test]
    fn creates_live_directories() {
        let Some((conn, _)) = connect("PYTHIA_TEST_DB_DBA") else {
            return;
        };
        let _accounts = crate::test_db::ACCOUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some((user, _)) = connect("PYTHIA_TEST_DB") else {
            return;
        };
        let grantee = user
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let cleanup = "begin
               for d in (select directory_name from all_directories
                          where directory_name = 'PYTHIA_OB_DIR') loop
                 execute immediate 'drop directory pythia_ob_dir';
               end loop;
             end;";
        conn.execute(cleanup, &[]).unwrap();
        let run = |statements: Vec<String>| {
            apply(&conn, &statements, None).unwrap_or_else(|e| {
                panic!("{}: {}", e.statement, e.error);
            })
        };
        let mut base = BaseLists::default();
        directory::lists(&conn, &mut base).unwrap();
        assert!(
            base.session_privileges
                .iter()
                .any(|p| p == "CREATE ANY DIRECTORY")
        );
        let def = directory::DirectoryDef {
            name: "pythia_ob_dir".into(),
            path: "/tmp/pythia_ob".into(),
        };
        run(directory::statements(None, &def).unwrap());
        let loaded = directory::load(&conn, "PYTHIA_OB_DIR").unwrap();
        assert_eq!(loaded.path, "/tmp/pythia_ob");
        assert_eq!(
            directory::statements(Some(&loaded), &loaded),
            Ok(Vec::new())
        );

        // Grants: read and write to the test user, then write taken away.
        let object = "directory PYTHIA_OB_DIR";
        let before = grants::load(&conn, "", "PYTHIA_OB_DIR", ObjectType::Directory).unwrap();
        assert_eq!(before.privileges, ["READ", "WRITE", "EXECUTE"]);
        let mut after = before.clone();
        after.rows.push(grants::GrantRow {
            grantee: grantee.clone(),
            granted: ["READ".to_owned(), "WRITE".to_owned()].into(),
            grantable: BTreeSet::new(),
        });
        run(grants::statements(object, &before, &after));
        let granted = grants::load(&conn, "", "PYTHIA_OB_DIR", ObjectType::Directory).unwrap();
        let row = granted.rows.iter().find(|r| r.grantee == grantee).unwrap();
        assert_eq!(row.granted, ["READ".to_owned(), "WRITE".to_owned()].into());
        let readable: Vec<String> = names(
            &user,
            "select directory_name from all_directories where directory_name = 'PYTHIA_OB_DIR'",
            &[],
        )
        .unwrap();
        assert_eq!(readable, ["PYTHIA_OB_DIR"]);

        // A new path keeps the grants.
        let moved = directory::DirectoryDef {
            path: "/tmp/pythia_ob2".into(),
            ..loaded.clone()
        };
        run(directory::statements(Some(&loaded), &moved).unwrap());
        assert_eq!(
            directory::load(&conn, "PYTHIA_OB_DIR").unwrap().path,
            "/tmp/pythia_ob2"
        );
        let kept = grants::load(&conn, "", "PYTHIA_OB_DIR", ObjectType::Directory).unwrap();
        let mut fewer = kept.clone();
        let row = fewer
            .rows
            .iter_mut()
            .find(|r| r.grantee == grantee)
            .unwrap();
        assert!(row.granted.remove("WRITE"));
        run(grants::statements(object, &kept, &fewer));
        let reloaded = grants::load(&conn, "", "PYTHIA_OB_DIR", ObjectType::Directory).unwrap();
        let row = reloaded.rows.iter().find(|r| r.grantee == grantee).unwrap();
        assert_eq!(row.granted, ["READ".to_owned()].into());
        conn.execute(cleanup, &[]).unwrap();
    }

    /// Profiles through the profile form's SQL (needs a DBA).
    #[test]
    fn creates_live_profiles() {
        let Some((conn, _)) = connect("PYTHIA_TEST_DB_DBA") else {
            return;
        };
        let _accounts = crate::test_db::ACCOUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cleanup = "begin
               for u in (select username from dba_users where username = 'PYTHIA_OB_PUSER') loop
                 execute immediate 'drop user pythia_ob_puser cascade';
               end loop;
               for p in (select distinct profile from dba_profiles
                          where profile = 'PYTHIA_OB_PROFILE') loop
                 execute immediate 'drop profile pythia_ob_profile cascade';
               end loop;
             end;";
        conn.execute(cleanup, &[]).unwrap();
        let run = |statements: Vec<String>| {
            apply(&conn, &statements, None).unwrap_or_else(|e| {
                panic!("{}: {}", e.statement, e.error);
            })
        };
        let mut base = BaseLists::default();
        profile::lists(&conn, &mut base).unwrap();
        assert!(
            base.session_privileges
                .iter()
                .any(|p| p == "CREATE PROFILE")
        );

        let mut def = profile::new_def(&conn);
        assert!(def.limits.iter().all(|l| l.value == "default"));
        let names: Vec<&str> = def.limits.iter().map(|l| l.resource.as_str()).collect();
        assert_eq!(names[0], "FAILED_LOGIN_ATTEMPTS", "password limits first");
        assert!(names.contains(&"SESSIONS_PER_USER"));
        def.name = "pythia_ob_profile".into();
        let set = |def: &mut profile::ProfileDef, resource: &str, value: &str| {
            def.limits
                .iter_mut()
                .find(|l| l.resource == resource)
                .unwrap()
                .value = value.into();
        };
        set(&mut def, "FAILED_LOGIN_ATTEMPTS", "4");
        set(&mut def, "PASSWORD_LOCK_TIME", "1/24");
        set(&mut def, "SESSIONS_PER_USER", "3");
        set(&mut def, "PRIVATE_SGA", "64K");
        if let Some(f) = base.verify_functions.first() {
            set(&mut def, "PASSWORD_VERIFY_FUNCTION", &f.to_lowercase());
        }
        run(profile::statements(None, &def).unwrap());
        let loaded = profile::load(&conn, "PYTHIA_OB_PROFILE").unwrap();
        let value = |def: &profile::ProfileDef, resource: &str| {
            def.limits
                .iter()
                .find(|l| l.resource == resource)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(value(&loaded, "FAILED_LOGIN_ATTEMPTS"), "4");
        assert!(value(&loaded, "PASSWORD_LOCK_TIME").starts_with("0.04"));
        assert_eq!(value(&loaded, "SESSIONS_PER_USER"), "3");
        assert_eq!(value(&loaded, "PRIVATE_SGA"), "65536");
        assert_eq!(value(&loaded, "IDLE_TIME"), "default");
        if let Some(f) = base.verify_functions.first() {
            assert_eq!(value(&loaded, "PASSWORD_VERIFY_FUNCTION"), *f);
        }
        assert!(loaded.users.is_empty());
        // Nothing to do on the loaded profile.
        assert_eq!(profile::statements(Some(&loaded), &loaded), Ok(Vec::new()));

        let mut edited = loaded.clone();
        set(&mut edited, "FAILED_LOGIN_ATTEMPTS", "default");
        set(&mut edited, "IDLE_TIME", "30");
        set(&mut edited, "PASSWORD_VERIFY_FUNCTION", "null");
        run(profile::statements(Some(&loaded), &edited).unwrap());
        conn.execute(
            "create user pythia_ob_puser identified by \"Pythia_ob_1\" profile pythia_ob_profile",
            &[],
        )
        .unwrap();
        let reloaded = profile::load(&conn, "PYTHIA_OB_PROFILE").unwrap();
        assert_eq!(value(&reloaded, "FAILED_LOGIN_ATTEMPTS"), "default");
        assert_eq!(value(&reloaded, "IDLE_TIME"), "30");
        assert_eq!(value(&reloaded, "PASSWORD_VERIFY_FUNCTION"), "null");
        assert_eq!(reloaded.users, ["PYTHIA_OB_PUSER"]);
        assert_eq!(
            profile::statements(Some(&reloaded), &reloaded),
            Ok(Vec::new())
        );
        conn.execute(cleanup, &[]).unwrap();
    }
}
