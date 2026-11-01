//! Dictionary lookups for code completion and navigation.
//!
//! `pythia_lsp_core` reports what it is missing as [`CatalogRequest`]s; each is
//! answered by a task on the connection's main session and stored in the
//! connection's [`Catalogs`]. Names follow Oracle's storage rules:
//! the engine already normalizes unquoted identifiers to uppercase.
//!
//! A SQL window's lookups go with its context (#320), the schema, database
//! or search path its session reports, and are kept by it: windows that
//! share one share the answers. What only the window's session sees, its
//! temporary tables and on SQLite the databases it attached, is looked up
//! there ([`WindowCatalog`]), while it is idle with no result open, and
//! kept for that window until its session reports its context again.

use std::collections::{HashMap, HashSet};

use pythia_db::oracle::Connection;
use pythia_db::{DbError, DbKind, DbRequest, Dialect, NameContext, TaskFn};
use pythia_lsp_core::{
    Catalog, CatalogCache, CatalogColumn, CatalogMember, CatalogObject, CatalogParam,
    CatalogRequest, Lookup, MemberKind, NameStyle, ParamMode,
};

/// Object types offered by completion and resolution.
const TYPES: &str = "'TABLE', 'VIEW', 'MATERIALIZED VIEW', 'SYNONYM', 'PACKAGE', 'PROCEDURE', 'FUNCTION', \
                     'SEQUENCE', 'TYPE'";

/// Result of one request, applied to the cache on the UI thread.
pub enum Answer {
    Schemas(Vec<String>),
    Objects(Option<String>, Vec<CatalogObject>),
    Resolved(Option<String>, String, Option<CatalogObject>),
    Columns(String, String, Vec<CatalogColumn>),
    Members(String, String, Vec<CatalogMember>),
}

impl Answer {
    pub fn apply(self, cache: &mut CatalogCache) {
        match self {
            Answer::Schemas(s) => cache.set_schemas(s),
            Answer::Objects(schema, objects) => cache.set_objects(schema.as_deref(), objects),
            Answer::Resolved(schema, name, object) => {
                cache.set_resolved(schema.as_deref(), &name, object)
            }
            Answer::Columns(owner, name, cols) => cache.set_columns(&owner, &name, cols),
            Answer::Members(owner, name, members) => cache.set_members(&owner, &name, members),
        }
    }

    /// What answers `request` when it can't be looked up: nothing, so
    /// that completion goes on without it.
    fn nothing(request: CatalogRequest) -> Self {
        match request {
            CatalogRequest::Schemas => Answer::Schemas(Vec::new()),
            CatalogRequest::Objects { schema } => Answer::Objects(schema, Vec::new()),
            CatalogRequest::Resolve { schema, name } => Answer::Resolved(schema, name, None),
            CatalogRequest::Columns { owner, name } => Answer::Columns(owner, name, Vec::new()),
            CatalogRequest::Members { owner, name } => Answer::Members(owner, name, Vec::new()),
        }
    }
}

/// Builds the task answering `request` on Oracle, in `context` (the
/// session's own when `None`).
pub fn task(request: CatalogRequest, context: Option<NameContext>) -> TaskFn {
    Box::new(move |conn: &Connection| {
        let answer = fetch(conn, request, context.as_ref())?;
        Ok(Box::new(answer) as Box<dyn std::any::Any + Send>)
    })
}

/// The request answering `request` (with an [`Answer`]) on a connection of
/// `dialect`, for names in `context` (the session's own when `None`).
pub fn request(
    id: u64,
    request: CatalogRequest,
    dialect: Dialect,
    context: Option<NameContext>,
) -> DbRequest {
    match dialect.kind() {
        DbKind::Oracle => DbRequest::Task {
            id,
            task: task(request, context),
        },
        DbKind::MySql | DbKind::Postgres | DbKind::SqlServer | DbKind::Sqlite => DbRequest::Run {
            id,
            task: Box::new(move |client| {
                let answer = dictionary::fetch(client, request, context.as_ref())?;
                Ok(Box::new(answer) as Box<dyn std::any::Any + Send>)
            }),
        },
    }
}

/// A connection's dictionary data for code assistance, looked up on its
/// main session: in that session's context, and in each other context a
/// SQL window's session reported (#320).
pub struct Catalogs {
    main: CatalogCache,
    /// The main session's context, which windows share unless theirs
    /// differs.
    context: NameContext,
    others: HashMap<NameContext, CatalogCache>,
    /// Answers nothing, for a context not looked up in yet.
    empty: CatalogCache,
    names: NameStyle,
}

impl Catalogs {
    /// The answers of the main session, in its `context`.
    pub fn new(context: NameContext, current_schema: String, names: NameStyle) -> Self {
        Self {
            main: CatalogCache::new(current_schema).with_names(names),
            context,
            others: HashMap::new(),
            empty: CatalogCache::new("").with_names(names),
            names,
        }
    }

    /// The main session's answers.
    pub fn main(&self) -> &CatalogCache {
        &self.main
    }

    /// The main session's context.
    pub fn context(&self) -> &NameContext {
        &self.context
    }

    /// What lookups in `context` (a window's) go with: none for the main
    /// session's own. SQLite's main session answers the same whatever a
    /// window attached: what only the window has is the window's to answer.
    pub fn key(&self, context: &NameContext, dialect: Dialect) -> Option<NameContext> {
        (context != &self.context && dialect.kind() != DbKind::Sqlite).then(|| context.clone())
    }

    /// The answers in `context` (`None`: the main session's).
    pub fn get(&self, context: Option<&NameContext>) -> &CatalogCache {
        match context {
            None => &self.main,
            Some(context) => self.others.get(context).unwrap_or(&self.empty),
        }
    }

    /// The answers in `context`, made on first use.
    pub fn get_mut(&mut self, context: Option<&NameContext>) -> &mut CatalogCache {
        let Some(context) = context else {
            return &mut self.main;
        };
        let names = self.names;
        self.others.entry(context.clone()).or_insert_with(|| {
            CatalogCache::new(context.current().unwrap_or_default()).with_names(names)
        })
    }

    /// Drops everything (after DDL, or on demand).
    pub fn clear(&mut self) {
        self.main.clear();
        self.others.clear();
    }
}

/// What a lookup a SQL window's code assistance misses needs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ask {
    /// The main session's answer, in the window's context.
    Main(CatalogRequest),
    /// The window's own session's.
    Own(OwnRequest),
}

/// What only a SQL window's own session sees (#320).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OwnRequest {
    /// The temporary tables it has: SQLite's `temp`, PostgreSQL's
    /// `pg_temp`, Oracle's private temporary tables.
    Temporary,
    /// The temporary table `name` of `schema`, if it has one, with its
    /// columns: MySQL lists them nowhere, and SQL Server's `#name` is the
    /// session's own.
    Probe {
        schema: Option<String>,
        name: String,
    },
    /// A temporary table's columns.
    Columns { owner: String, name: String },
    /// SQLite: a lookup in the databases the window attached, which the
    /// main session hasn't.
    Attached(CatalogRequest),
}

impl OwnRequest {
    /// What answers it when it can't be looked up: nothing.
    pub fn nothing(self) -> OwnAnswer {
        match self {
            OwnRequest::Temporary => OwnAnswer::Temporary(Vec::new()),
            OwnRequest::Probe { schema, name } => OwnAnswer::Probed(schema, name, None),
            OwnRequest::Columns { owner, name } => OwnAnswer::Columns(owner, name, Vec::new()),
            OwnRequest::Attached(request) => OwnAnswer::Attached(Answer::nothing(request)),
        }
    }
}

/// The answer of an [`OwnRequest`].
pub enum OwnAnswer {
    Temporary(Vec<CatalogObject>),
    Probed(
        Option<String>,
        String,
        Option<(CatalogObject, Vec<CatalogColumn>)>,
    ),
    Columns(String, String, Vec<CatalogColumn>),
    Attached(Answer),
}

/// The schema a window's temporary tables are kept under in its
/// [`Window`]'s cache.
const TEMPORARY: &str = "\0temporary";

/// The prefix of Oracle's private temporary tables (its default).
const PRIVATE_TEMPORARY: &str = "ORA$PTT_";

/// A SQL window's context, and what only its session sees (#320).
pub struct Window {
    /// The connection whose session it is.
    pub connection: String,
    pub context: NameContext,
    /// Its temporary tables, as objects of [`TEMPORARY`], and their
    /// columns; MySQL's and SQL Server's as resolved names.
    temporary: CatalogCache,
    /// SQLite: what the databases it attached have.
    attached: CatalogCache,
    requested: HashSet<OwnRequest>,
}

impl Window {
    pub fn new(connection: String, context: NameContext, names: NameStyle) -> Self {
        Self {
            connection,
            context,
            temporary: CatalogCache::new("").with_names(names),
            attached: CatalogCache::new("main").with_names(names),
            requested: HashSet::new(),
        }
    }

    /// Forgets what its session told (another session, or a statement that
    /// changed its context or made tables).
    pub fn clear(&mut self) {
        self.temporary.clear();
        self.attached.clear();
        self.requested.clear();
    }

    /// Notes `request` as asked; false when it was before.
    pub fn ask(&mut self, request: &OwnRequest) -> bool {
        self.requested.insert(request.clone())
    }

    pub fn apply(&mut self, answer: OwnAnswer) {
        match answer {
            OwnAnswer::Temporary(objects) => self.temporary.set_objects(Some(TEMPORARY), objects),
            OwnAnswer::Probed(schema, name, found) => {
                let object = found.map(|(object, columns)| {
                    self.temporary
                        .set_columns(&object.owner, &object.name, columns);
                    object
                });
                self.temporary
                    .set_resolved(schema.as_deref(), &name, object);
            }
            OwnAnswer::Columns(owner, name, columns) => {
                self.temporary.set_columns(&owner, &name, columns)
            }
            OwnAnswer::Attached(answer) => answer.apply(&mut self.attached),
        }
    }
}

/// What a SQL window's code assistance reads (#320): the main session's
/// answers in the window's context, and over them its own session's, of
/// what only it sees. Those are asked for only while it is `live`, idle
/// with no result open; else completion goes without them.
pub struct WindowCatalog<'a> {
    shared: &'a CatalogCache,
    window: Option<&'a Window>,
    /// The main session's context.
    main: &'a NameContext,
    kind: DbKind,
    live: bool,
}

impl<'a> WindowCatalog<'a> {
    /// The catalog of a window with `window`'s session (none yet: the main
    /// session's context) on a connection with `catalogs`.
    pub fn new(
        catalogs: &'a Catalogs,
        window: Option<&'a Window>,
        dialect: Dialect,
        live: bool,
    ) -> Self {
        let key = window.and_then(|w| catalogs.key(&w.context, dialect));
        Self {
            shared: catalogs.get(key.as_ref()),
            window,
            main: catalogs.context(),
            kind: dialect.kind(),
            live,
        }
    }

    /// What `request`, which code assistance missed, needs asking for.
    pub fn asks(&self, request: &CatalogRequest) -> Vec<Ask> {
        let missing = match request {
            CatalogRequest::Schemas => self.schemas_in().err(),
            CatalogRequest::Objects { schema } => self.objects_in(schema.as_deref()).err(),
            CatalogRequest::Resolve { schema, name } => {
                self.resolve_in(schema.as_deref(), name).err()
            }
            CatalogRequest::Columns { owner, name } => self.columns_in(owner, name).err(),
            CatalogRequest::Members { owner, name } => self.members_in(owner, name).err(),
        };
        missing.unwrap_or_default()
    }

    /// `lookup` of the window's own session: `Ok(None)` when it isn't there
    /// and can't be asked for now.
    fn own<T>(
        &self,
        lookup: Lookup<T>,
        request: impl FnOnce() -> OwnRequest,
    ) -> Result<Option<T>, Vec<Ask>> {
        match lookup {
            Lookup::Ready(value) => Ok(Some(value)),
            Lookup::Pending if self.live => Err(vec![Ask::Own(request())]),
            Lookup::Pending => Ok(None),
        }
    }

    /// `lookup` of the main session.
    fn from_main<T>(
        lookup: Lookup<T>,
        request: impl FnOnce() -> CatalogRequest,
    ) -> Result<T, Vec<Ask>> {
        match lookup {
            Lookup::Ready(value) => Ok(value),
            Lookup::Pending => Err(vec![Ask::Main(request())]),
        }
    }

    /// Whether the window's database lists its temporary tables, and a name
    /// of `schema` may be one: SQLite's and PostgreSQL's come first for
    /// names without a schema, Oracle's have their prefix.
    fn lists_temporary(&self, schema: Option<&str>, name: &str) -> bool {
        match self.kind {
            DbKind::Sqlite => schema.is_none_or(|s| s.eq_ignore_ascii_case("temp")),
            DbKind::Postgres => schema.is_none_or(is_pg_temp),
            DbKind::Oracle => name.starts_with(PRIVATE_TEMPORARY),
            DbKind::MySql | DbKind::SqlServer => false,
        }
    }

    /// The window's temporary tables, where its database lists them.
    fn temporary(&self) -> Result<Option<Vec<CatalogObject>>, Vec<Ask>> {
        let Some(window) = self.window else {
            return Ok(None);
        };
        self.own(window.temporary.objects(Some(TEMPORARY)), || {
            OwnRequest::Temporary
        })
    }

    /// The window's temporary table `name` of `schema`.
    fn temporary_named(
        &self,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Option<CatalogObject>, Vec<Ask>> {
        let Some(window) = self.window else {
            return Ok(None);
        };
        if self.temporary()?.is_none() {
            return Ok(None);
        }
        let Lookup::Ready(found) = window.temporary.resolve(Some(TEMPORARY), name) else {
            return Ok(None);
        };
        // Oracle's are names of their owner's schema.
        let current = window.context.current().unwrap_or_default();
        Ok(found.filter(|o| self.kind != DbKind::Oracle || o.owner == schema.unwrap_or(current)))
    }

    /// The temporary table `name` of `schema` probed for in the window's
    /// session (MySQL, SQL Server's `#name`).
    fn probed(&self, schema: Option<&str>, name: &str) -> Result<Option<CatalogObject>, Vec<Ask>> {
        let Some(window) = self.window else {
            return Ok(None);
        };
        let found = self.own(window.temporary.resolve(schema, name), || {
            OwnRequest::Probe {
                schema: schema.map(str::to_owned),
                name: name.to_owned(),
            }
        })?;
        Ok(found.flatten())
    }

    /// Whether `owner`.`name` is one of the window's temporary tables,
    /// whose columns its session knows: the main session's doesn't, or not
    /// before the window's transaction that made it ends (PostgreSQL).
    fn is_temporary(&self, owner: &str, name: &str) -> bool {
        let Some(window) = self.window else {
            return false;
        };
        match self.kind {
            DbKind::Sqlite => owner.eq_ignore_ascii_case("temp"),
            DbKind::SqlServer => name.starts_with('#'),
            DbKind::MySql => matches!(window.temporary.columns(owner, name), Lookup::Ready(_)),
            DbKind::Oracle => matches!(
                window.temporary.objects(Some(TEMPORARY)),
                Lookup::Ready(objects) if objects.iter().any(|o| o.owner == owner && o.name == name)
            ),
            DbKind::Postgres => is_pg_temp(owner),
        }
    }

    /// SQLite: whether a lookup in `schema` is the window's own: a database
    /// only it attached, or any (`None`) once its databases differ from the
    /// main session's.
    fn attached_own(&self, schema: Option<&str>) -> bool {
        let Some(window) = self.window.filter(|_| self.kind == DbKind::Sqlite) else {
            return false;
        };
        match schema {
            None => window.context.attached != self.main.attached,
            Some(schema) => {
                let named = |attached: &'a [(String, String)]| {
                    attached
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(schema))
                };
                let own = named(&window.context.attached);
                own.is_some() && own != named(&self.main.attached)
            }
        }
    }

    fn schemas_in(&self) -> Result<Vec<String>, Vec<Ask>> {
        if let Some(window) = self.window.filter(|_| self.attached_own(None))
            && let Some(schemas) = self.own(window.attached.schemas(), || {
                OwnRequest::Attached(CatalogRequest::Schemas)
            })?
        {
            return Ok(schemas);
        }
        Self::from_main(self.shared.schemas(), || CatalogRequest::Schemas)
    }

    fn objects_in(&self, schema: Option<&str>) -> Result<Vec<CatalogObject>, Vec<Ask>> {
        let request = || CatalogRequest::Objects {
            schema: schema.map(str::to_owned),
        };
        match schema {
            Some(s)
                if (self.kind == DbKind::Sqlite && s.eq_ignore_ascii_case("temp"))
                    || (self.kind == DbKind::Postgres && is_pg_temp(s)) =>
            {
                return Ok(self.temporary()?.unwrap_or_default());
            }
            Some(_) if self.attached_own(schema) => {
                let window = self.window.expect("attached_own");
                return Ok(self
                    .own(window.attached.objects(schema), || {
                        OwnRequest::Attached(request())
                    })?
                    .unwrap_or_default());
            }
            Some(_) => return Self::from_main(self.shared.objects(schema), request),
            None => {}
        }
        // Unqualified names find the temporary tables too (SQLite,
        // PostgreSQL); Oracle's private ones once they were looked up.
        let temporary = match self.kind {
            DbKind::Sqlite | DbKind::Postgres => self.temporary(),
            DbKind::Oracle => {
                Ok(self
                    .window
                    .and_then(|w| match w.temporary.objects(Some(TEMPORARY)) {
                        Lookup::Ready(objects) => Some(objects),
                        Lookup::Pending => None,
                    }))
            }
            DbKind::MySql | DbKind::SqlServer => Ok(None),
        };
        let objects = Self::from_main(self.shared.objects(None), request);
        match (objects, temporary) {
            (Ok(mut objects), Ok(temporary)) => {
                let current = self.shared.current_schema();
                objects.extend(
                    temporary
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|o| self.kind != DbKind::Oracle || o.owner == current),
                );
                Ok(objects)
            }
            (Err(mut asks), Err(more)) => {
                asks.extend(more);
                Err(asks)
            }
            (Err(asks), _) | (_, Err(asks)) => Err(asks),
        }
    }

    fn resolve_in(
        &self,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Option<CatalogObject>, Vec<Ask>> {
        // SQL Server's `#name` is the session's own.
        if self.kind == DbKind::SqlServer && name.starts_with('#') {
            return self.probed(schema, name);
        }
        let mut asks = Vec::new();
        // The window's temporary tables come first.
        if self.lists_temporary(schema, name) {
            match self.temporary_named(schema, name) {
                Ok(Some(found)) => return Ok(Some(found)),
                Ok(None) => {}
                Err(more) => asks.extend(more),
            }
        }
        let request = || CatalogRequest::Resolve {
            schema: schema.map(str::to_owned),
            name: name.to_owned(),
        };
        if let Some(window) = self.window.filter(|_| self.attached_own(schema)) {
            match self.own(window.attached.resolve(schema, name), || {
                OwnRequest::Attached(request())
            }) {
                Ok(Some(found)) if asks.is_empty() => return Ok(found),
                Ok(Some(_)) => return Err(asks),
                // A database only the window has, which it can't be asked
                // about now.
                Ok(None) if schema.is_some() => return Ok(None),
                Ok(None) => {}
                Err(more) => {
                    asks.extend(more);
                    return Err(asks);
                }
            }
        }
        match self.shared.resolve(schema, name) {
            // Not in the dictionary: a temporary table of MySQL's maybe.
            Lookup::Ready(None) if self.kind == DbKind::MySql && asks.is_empty() => {
                self.probed(schema, name)
            }
            Lookup::Ready(found) if asks.is_empty() => Ok(found),
            Lookup::Ready(_) => Err(asks),
            Lookup::Pending => {
                asks.push(Ask::Main(request()));
                Err(asks)
            }
        }
    }

    fn columns_in(&self, owner: &str, name: &str) -> Result<Vec<CatalogColumn>, Vec<Ask>> {
        let own = |cache: &CatalogCache, request: OwnRequest| {
            self.own(cache.columns(owner, name), || request)
                .map(Option::unwrap_or_default)
        };
        if let Some(window) = self.window {
            if self.is_temporary(owner, name) {
                return own(
                    &window.temporary,
                    OwnRequest::Columns {
                        owner: owner.to_owned(),
                        name: name.to_owned(),
                    },
                );
            }
            if self.attached_own(Some(owner)) {
                return own(
                    &window.attached,
                    OwnRequest::Attached(CatalogRequest::Columns {
                        owner: owner.to_owned(),
                        name: name.to_owned(),
                    }),
                );
            }
        }
        Self::from_main(self.shared.columns(owner, name), || {
            CatalogRequest::Columns {
                owner: owner.to_owned(),
                name: name.to_owned(),
            }
        })
    }

    fn members_in(&self, owner: &str, name: &str) -> Result<Vec<CatalogMember>, Vec<Ask>> {
        // A temporary table or a table of an attached database has none.
        if self.is_temporary(owner, name) || self.attached_own(Some(owner)) {
            return Ok(Vec::new());
        }
        Self::from_main(self.shared.members(owner, name), || {
            CatalogRequest::Members {
                owner: owner.to_owned(),
                name: name.to_owned(),
            }
        })
    }
}

/// PostgreSQL's names of a session's temporary schema.
fn is_pg_temp(schema: &str) -> bool {
    let schema = schema.to_ascii_lowercase();
    schema == "pg_temp" || schema.starts_with("pg_temp_")
}

impl Catalog for WindowCatalog<'_> {
    fn current_schema(&self) -> &str {
        self.shared.current_schema()
    }

    fn names(&self) -> NameStyle {
        self.shared.names()
    }

    fn schemas(&self) -> Lookup<Vec<String>> {
        ready(self.schemas_in())
    }

    fn objects(&self, schema: Option<&str>) -> Lookup<Vec<CatalogObject>> {
        ready(self.objects_in(schema))
    }

    fn resolve(&self, schema: Option<&str>, name: &str) -> Lookup<Option<CatalogObject>> {
        ready(self.resolve_in(schema, name))
    }

    fn columns(&self, owner: &str, name: &str) -> Lookup<Vec<CatalogColumn>> {
        ready(self.columns_in(owner, name))
    }

    fn members(&self, owner: &str, name: &str) -> Lookup<Vec<CatalogMember>> {
        ready(self.members_in(owner, name))
    }
}

fn ready<T>(found: Result<T, Vec<Ask>>) -> Lookup<T> {
    match found {
        Ok(value) => Lookup::Ready(value),
        Err(_) => Lookup::Pending,
    }
}

/// The request answering `request` (with an [`OwnAnswer`]) in a window's
/// session on a connection of `dialect`.
pub fn own_request(id: u64, request: OwnRequest, dialect: Dialect) -> DbRequest {
    match dialect.kind() {
        DbKind::Oracle => DbRequest::Task {
            id,
            task: Box::new(move |conn: &Connection| {
                let answer = own_fetch(conn, request)?;
                Ok(Box::new(answer) as Box<dyn std::any::Any + Send>)
            }),
        },
        DbKind::MySql | DbKind::Postgres | DbKind::SqlServer | DbKind::Sqlite => DbRequest::Run {
            id,
            task: Box::new(move |client| {
                let answer = dictionary::own(client, request)?;
                Ok(Box::new(answer) as Box<dyn std::any::Any + Send>)
            }),
        },
    }
}

/// Oracle: the session's private temporary tables, and their columns,
/// which no dictionary view has: described from a query.
fn own_fetch(conn: &Connection, request: OwnRequest) -> Result<OwnAnswer, DbError> {
    Ok(match request {
        OwnRequest::Temporary => {
            let rows = conn.query_as::<(String, String)>(
                "select owner, table_name from user_private_temp_tables \
                  where sid = sys_context('USERENV', 'SID')",
                &[],
            )?;
            let mut objects = Vec::new();
            for row in rows {
                let (owner, name) = row?;
                objects.push(CatalogObject {
                    owner,
                    name,
                    object_type: "TABLE".into(),
                });
            }
            OwnAnswer::Temporary(objects)
        }
        OwnRequest::Columns { owner, name } => {
            let quoted = |n: &str| format!("\"{}\"", n.replace('"', "\"\""));
            let rows = conn.query(
                &format!(
                    "select * from {}.{} where 1 = 0",
                    quoted(&owner),
                    quoted(&name)
                ),
                &[],
            )?;
            let columns = rows
                .column_info()
                .iter()
                .map(|c| CatalogColumn {
                    name: c.name().to_owned(),
                    data_type: c.oracle_type().to_string(),
                    nullable: c.nullable(),
                    comment: None,
                })
                .collect();
            OwnAnswer::Columns(owner, name, columns)
        }
        request => request.nothing(),
    })
}

/// The dictionaries of MySQL, PostgreSQL, SQL Server and SQLite. Code
/// assistance asks with uppercase names (Oracle's rule for unquoted ones);
/// these databases keep their case (PostgreSQL folds unquoted names to
/// lowercase), so names are compared without it and answered as stored.
mod dictionary {
    use std::borrow::Cow;

    use pythia_db::{
        Cell, Client, DbError, DbKind, MetaQuery, MetaResult, NameContext, ObjectType,
    };
    use pythia_lsp_core::{CatalogColumn, CatalogObject, CatalogRequest};

    use super::{Answer, OwnAnswer, OwnRequest};

    /// A database's queries; parameters are written by [`Sql::param`].
    struct Sql {
        kind: DbKind,
        schemas: &'static str,
        /// The schema unqualified names are looked up in.
        current: &'static str,
        /// Tables, views and routines as `(owner, name, type)`.
        objects: &'static str,
        /// What makes a query return one row.
        first_row: &'static str,
        /// `(name, type, nullable, comment)` of the table of schema `1` and
        /// name `2`.
        columns: &'static str,
    }

    impl Sql {
        fn of(kind: DbKind) -> &'static Sql {
            match kind {
                DbKind::Postgres => &POSTGRES,
                DbKind::SqlServer => &SQLSERVER,
                _ => &MYSQL,
            }
        }

        /// Parameter `n` (from 1) as the database writes it.
        fn param(&self, n: usize) -> String {
            match self.kind {
                DbKind::Postgres => format!("${n}"),
                DbKind::SqlServer => format!("@P{n}"),
                _ => "?".into(),
            }
        }
    }

    const MYSQL: Sql = Sql {
        kind: DbKind::MySql,
        schemas: "select schema_name from information_schema.schemata order by 1",
        current: "select database()",
        // MariaDB's sequences are sequences (#270).
        objects: "select table_schema as owner, table_name as name, \
                         case table_type when 'VIEW' then 'VIEW' \
                                         when 'SEQUENCE' then 'SEQUENCE' else 'TABLE' end as type \
                    from information_schema.tables \
                  union all \
                  select routine_schema, routine_name, routine_type \
                    from information_schema.routines",
        first_row: "limit 1",
        columns: "select column_name, column_type, is_nullable, nullif(column_comment, '') \
                    from information_schema.columns \
                   where lower(table_schema) = lower(?) and lower(table_name) = lower(?) \
                   order by ordinal_position",
    };

    const POSTGRES: Sql = Sql {
        kind: DbKind::Postgres,
        schemas: "select nspname from pg_namespace \
                   where nspname not like 'pg\\_toast%' and nspname not like 'pg\\_temp\\_%' \
                   order by 1",
        current: "select current_schema()",
        objects: "select n.nspname as owner, c.relname as name, \
                         case c.relkind when 'v' then 'VIEW' when 'm' then 'MATERIALIZED VIEW' \
                              else 'TABLE' end as type \
                    from pg_class c join pg_namespace n on n.oid = c.relnamespace \
                   where c.relkind in ('r', 'p', 'f', 'v', 'm') and not c.relispartition \
                  union \
                  select n.nspname, p.proname, \
                         case p.prokind when 'p' then 'PROCEDURE' else 'FUNCTION' end \
                    from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
                   where p.prokind in ('f', 'p')",
        first_row: "limit 1",
        // The table whose name matches best: exactly, then as folded.
        columns: "select a.attname, format_type(a.atttypid, a.atttypmod), \
                         case when a.attnotnull then 'NO' else 'YES' end, \
                         col_description(a.attrelid, a.attnum) \
                    from pg_attribute a \
                   where a.attnum > 0 and not a.attisdropped \
                     and a.attrelid = (select c.oid from pg_class c \
                                         join pg_namespace n on n.oid = c.relnamespace \
                                        where lower(n.nspname) = lower($1) \
                                          and lower(c.relname) = lower($2) \
                                        order by (c.relname = $2) desc, \
                                                 (c.relname = lower($2)) desc \
                                        limit 1) \
                   order by a.attnum",
    };

    const SQLSERVER: Sql = Sql {
        kind: DbKind::SqlServer,
        schemas: "select name from sys.schemas where schema_id < 16384 or schema_id > 16399 \
                   order by 1",
        current: "select schema_name()",
        objects: "select s.name as owner, o.name as name, \
                         case when o.type = 'U' then 'TABLE' when o.type = 'V' then 'VIEW' \
                              when o.type in ('P', 'PC') then 'PROCEDURE' \
                              else 'FUNCTION' end as type \
                    from sys.objects o join sys.schemas s on s.schema_id = o.schema_id \
                   where o.type in ('U', 'V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT') \
                     and o.is_ms_shipped = 0",
        first_row: "offset 0 rows fetch next 1 rows only",
        columns: "select column_name, \
                         data_type + case when character_maximum_length = -1 then '(max)' \
                              when character_maximum_length is not null \
                                then '(' + cast(character_maximum_length as varchar(10)) + ')' \
                              when data_type in ('decimal', 'numeric') \
                                then '(' + cast(numeric_precision as varchar(10)) + ',' \
                                     + cast(numeric_scale as varchar(10)) + ')' \
                              else '' end, \
                         is_nullable, null \
                    from information_schema.columns \
                   where table_schema = @P1 and table_name = @P2 \
                   order by ordinal_position",
    };

    /// Answers `request` for names in `context` (the session's own when
    /// `None`): SQL Server's database, the schemas MySQL's, PostgreSQL's and
    /// SQL Server's unqualified names are looked up in (#320).
    pub(super) fn fetch(
        client: &mut dyn Client,
        request: CatalogRequest,
        context: Option<&NameContext>,
    ) -> Result<Answer, DbError> {
        if client.dialect().kind() == DbKind::Sqlite {
            return sqlite(client, request);
        }
        let sql = Sql::of(client.dialect().kind());
        // SQL Server's views of another database are named with it.
        let database = context
            .map(|c| c.database.as_str())
            .filter(|d| !d.is_empty() && sql.kind == DbKind::SqlServer);
        let of = |query: &'static str| -> Cow<'static, str> {
            match database {
                Some(database) => Cow::Owned(in_database(query, database)),
                None => Cow::Borrowed(query),
            }
        };
        Ok(match request {
            CatalogRequest::Schemas => {
                let rows = client.query(&of(sql.schemas), &[])?;
                Answer::Schemas(rows.into_iter().filter_map(first).collect())
            }
            CatalogRequest::Objects { schema } => {
                let owner = match &schema {
                    Some(schema) => schema.clone(),
                    None => owners(client, sql, context)?
                        .into_iter()
                        .next()
                        .unwrap_or_default(),
                };
                let rows = client.query(
                    &format!(
                        "select * from ({}) o where lower(owner) = lower({})",
                        of(sql.objects),
                        sql.param(1)
                    ),
                    &[&owner],
                )?;
                Answer::Objects(schema, rows.into_iter().filter_map(object).collect())
            }
            CatalogRequest::Resolve { schema, name } => {
                // Unqualified: in each schema the database looks in, in
                // turn.
                let owners = match &schema {
                    Some(schema) => vec![schema.clone()],
                    None => owners(client, sql, context)?,
                };
                let query = format!(
                    "select * from ({}) o \
                      where lower(owner) = lower({}) and lower(name) = lower({}) \
                      order by case when name = {} then 0 else 1 end, \
                               case when type = 'TABLE' then 0 else 1 end {}",
                    of(sql.objects),
                    sql.param(1),
                    sql.param(2),
                    sql.param(3),
                    sql.first_row
                );
                let mut object = None;
                for owner in owners {
                    let rows = client.query(&query, &[&owner, &name, &name])?;
                    object = rows.into_iter().find_map(self::object);
                    if object.is_some() {
                        break;
                    }
                }
                Answer::Resolved(schema, name, object)
            }
            CatalogRequest::Columns { owner, name } => {
                let rows = client.query(&of(sql.columns), &[&owner, &name])?;
                let columns = rows
                    .into_iter()
                    .filter_map(|mut r| {
                        let comment = r.get_mut(3).and_then(Option::take);
                        Some(CatalogColumn {
                            name: r.first()?.clone()?,
                            data_type: r.get(1)?.clone()?,
                            nullable: r.get(2)?.as_deref() == Some("YES"),
                            comment,
                        })
                    })
                    .collect();
                Answer::Columns(owner, name, columns)
            }
            // Routines stand alone: no packages with members.
            CatalogRequest::Members { owner, name } => Answer::Members(owner, name, Vec::new()),
        })
    }

    /// SQLite: each attached database has a dictionary of its own, so the
    /// session's dictionary lookups answer.
    /// SQLite: the tables and views of the attached database `owner`.
    fn sqlite_tables(client: &mut dyn Client, owner: &str) -> Result<Vec<CatalogObject>, DbError> {
        let mut out = Vec::new();
        for object_type in [ObjectType::Table, ObjectType::View] {
            let found = client.meta(&MetaQuery::Objects {
                owner: owner.to_owned(),
                object_type,
                filter: None,
            })?;
            if let MetaResult::Objects(objects) = found {
                out.extend(objects.into_iter().map(|o| CatalogObject {
                    owner: o.owner,
                    name: o.name,
                    object_type: o.object_type.dictionary_name().to_owned(),
                }));
            }
        }
        Ok(out)
    }

    fn sqlite(client: &mut dyn Client, request: CatalogRequest) -> Result<Answer, DbError> {
        Ok(match request {
            CatalogRequest::Schemas => match client.meta(&MetaQuery::Schemas)? {
                MetaResult::Schemas(schemas) => Answer::Schemas(schemas),
                _ => Answer::Schemas(Vec::new()),
            },
            CatalogRequest::Objects { schema } => {
                let objects = sqlite_tables(client, schema.as_deref().unwrap_or("main"))?;
                Answer::Objects(schema, objects)
            }
            CatalogRequest::Resolve { schema, name } => {
                // Unqualified: where SQLite looks, `temp`, `main`, then the
                // attached databases in the order they were attached (#236).
                let owners: Vec<String> = match &schema {
                    Some(schema) => vec![schema.clone()],
                    None => {
                        let mut owners = vec!["temp".to_owned(), "main".to_owned()];
                        if let MetaResult::Schemas(schemas) = client.meta(&MetaQuery::Schemas)? {
                            owners
                                .extend(schemas.into_iter().filter(|s| s != "temp" && s != "main"));
                        }
                        owners
                    }
                };
                let mut object = None;
                for owner in owners {
                    let mut found = sqlite_tables(client, &owner)?;
                    found.retain(|o| o.name.eq_ignore_ascii_case(&name));
                    // An exact match first, then tables before views.
                    found.sort_by_key(|o| (o.name != name, o.object_type != "TABLE"));
                    if let Some(first) = found.into_iter().next() {
                        object = Some(first);
                        break;
                    }
                }
                Answer::Resolved(schema, name, object)
            }
            CatalogRequest::Columns { owner, name } => {
                let found = client.meta(&MetaQuery::Columns {
                    owner: owner.clone(),
                    table: name.clone(),
                    keys: false,
                })?;
                let columns = match found {
                    MetaResult::Columns(columns) => columns
                        .into_iter()
                        .map(|c| CatalogColumn {
                            name: c.name,
                            data_type: c.data_type,
                            nullable: c.nullable,
                            comment: None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                Answer::Columns(owner, name, columns)
            }
            CatalogRequest::Members { owner, name } => Answer::Members(owner, name, Vec::new()),
        })
    }

    /// The schemas unqualified names are looked up in, the current one
    /// (MySQL's database) first: `context`'s, or the session's. SQL Server
    /// looks in `dbo` after the default schema.
    fn owners(
        client: &mut dyn Client,
        sql: &Sql,
        context: Option<&NameContext>,
    ) -> Result<Vec<String>, DbError> {
        let mut owners = match context {
            Some(context) => context.schemas.clone(),
            None => {
                let rows = client.query(sql.current, &[])?;
                vec![rows.into_iter().find_map(first).unwrap_or_default()]
            }
        };
        if sql.kind == DbKind::SqlServer && !owners.iter().any(|o| o.eq_ignore_ascii_case("dbo")) {
            owners.push("dbo".into());
        }
        Ok(owners)
    }

    /// SQL Server's catalog views in `query` as those of `database`.
    fn in_database(query: &str, database: &str) -> String {
        let database = format!("[{}]", database.replace(']', "]]"));
        query
            .replace(" sys.", &format!(" {database}.sys."))
            .replace(
                " information_schema.",
                &format!(" {database}.information_schema."),
            )
    }

    /// Answers a lookup of what only a SQL window's session sees (#320).
    pub(super) fn own(client: &mut dyn Client, request: OwnRequest) -> Result<OwnAnswer, DbError> {
        guarded(client, |client| own_unguarded(client, request))
    }

    /// The savepoint lookups in a SQL window's open transaction run after.
    const SAVEPOINT: &str = "pythia_lookup";

    /// Runs `lookup` so that it leaves the session's open transaction as it
    /// was, also when it fails (#320): where a failed statement would abort
    /// it (PostgreSQL), after a savepoint, rolled back to when it fails, as
    /// the session does for a SQL window's dictionary queries since #269.
    pub(super) fn guarded<T>(
        client: &mut dyn Client,
        lookup: impl FnOnce(&mut dyn Client) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        if !client.aborts_transaction() {
            return lookup(client);
        }
        client.execute(&format!("savepoint {SAVEPOINT}"))?;
        let result = lookup(client);
        let back = if result.is_ok() {
            format!("release savepoint {SAVEPOINT}")
        } else {
            format!("rollback to savepoint {SAVEPOINT}; release savepoint {SAVEPOINT}")
        };
        client.execute(&back)?;
        result
    }

    fn own_unguarded(client: &mut dyn Client, request: OwnRequest) -> Result<OwnAnswer, DbError> {
        let kind = client.dialect().kind();
        Ok(match (kind, request) {
            (DbKind::Sqlite, OwnRequest::Temporary) => {
                OwnAnswer::Temporary(sqlite_tables(client, "temp")?)
            }
            (DbKind::Sqlite, OwnRequest::Columns { owner, name }) => {
                let Answer::Columns(owner, name, columns) =
                    sqlite(client, CatalogRequest::Columns { owner, name })?
                else {
                    unreachable!("columns answer columns")
                };
                OwnAnswer::Columns(owner, name, columns)
            }
            (DbKind::Sqlite, OwnRequest::Attached(request)) => {
                OwnAnswer::Attached(sqlite(client, request)?)
            }
            (DbKind::Postgres, OwnRequest::Columns { owner, name }) => {
                let Answer::Columns(owner, name, columns) =
                    fetch(client, CatalogRequest::Columns { owner, name }, None)?
                else {
                    unreachable!("columns answer columns")
                };
                OwnAnswer::Columns(owner, name, columns)
            }
            (DbKind::Postgres, OwnRequest::Temporary) => {
                let rows = client.query(
                    "select n.nspname, c.relname, \
                            case c.relkind when 'v' then 'VIEW' else 'TABLE' end \
                       from pg_class c join pg_namespace n on n.oid = c.relnamespace \
                      where c.relnamespace = pg_my_temp_schema() \
                        and c.relkind in ('r', 'p', 'f', 'v') and not c.relispartition",
                    &[],
                )?;
                OwnAnswer::Temporary(rows.into_iter().filter_map(object).collect())
            }
            (DbKind::MySql, OwnRequest::Probe { schema, name }) => {
                let found = mysql_temporary(client, schema.as_deref(), &name)?;
                OwnAnswer::Probed(schema, name, found)
            }
            (DbKind::SqlServer, OwnRequest::Probe { schema, name }) => {
                let found = sqlserver_temporary(client, &name)?;
                OwnAnswer::Probed(schema, name, found)
            }
            (_, request) => request.nothing(),
        })
    }

    /// MySQL: the temporary table `name` of `schema` (the database in use
    /// when `None`), which no dictionary lists: SHOW CREATE TABLE says
    /// `CREATE TEMPORARY TABLE` of one, which hides a table of its name.
    /// Unquoted names come uppercase: as written, else in lowercase.
    fn mysql_temporary(
        client: &mut dyn Client,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Option<(CatalogObject, Vec<CatalogColumn>)>, DbError> {
        let quoted = |n: &str| format!("`{}`", n.replace('`', "``"));
        let owner = match schema {
            Some(schema) => schema.to_owned(),
            None => client
                .query("select database()", &[])?
                .into_iter()
                .find_map(first)
                .unwrap_or_default(),
        };
        let lower = name.to_lowercase();
        for name in [name, lower.as_str()] {
            let table = match schema {
                Some(schema) => format!("{}.{}", quoted(schema), quoted(name)),
                None => quoted(name),
            };
            // Not there (ERROR 1146): no table of this name.
            let Ok(rows) = client.query(&format!("show create table {table}"), &[]) else {
                continue;
            };
            let Some(mut row) = rows.into_iter().next() else {
                continue;
            };
            let create = row.get_mut(1).and_then(Option::take).unwrap_or_default();
            if !create.starts_with("CREATE TEMPORARY TABLE") {
                return Ok(None);
            }
            let stored = row
                .first_mut()
                .and_then(Option::take)
                .unwrap_or_else(|| name.to_owned());
            // Field, Type, Collation, Null, Key, Default, Extra, Privileges,
            // Comment.
            let columns = client
                .query(&format!("show full columns from {table}"), &[])?
                .into_iter()
                .filter_map(|mut r| {
                    let comment = r
                        .get_mut(8)
                        .and_then(Option::take)
                        .filter(|c| !c.is_empty());
                    Some(CatalogColumn {
                        name: r.first()?.clone()?,
                        data_type: r.get(1)?.clone()?,
                        nullable: r.get(3)?.as_deref() == Some("YES"),
                        comment,
                    })
                })
                .collect();
            let object = CatalogObject {
                owner,
                name: stored,
                object_type: "TABLE".into(),
            };
            return Ok(Some((object, columns)));
        }
        Ok(None)
    }

    /// SQL Server: the session's temporary table `#name`, which is the
    /// session's own in `tempdb`, with its columns.
    fn sqlserver_temporary(
        client: &mut dyn Client,
        name: &str,
    ) -> Result<Option<(CatalogObject, Vec<CatalogColumn>)>, DbError> {
        let rows = client.query(
            "select c.name, type_name(c.user_type_id), c.max_length, c.precision, c.scale, \
                    c.is_nullable \
               from tempdb.sys.columns c \
              where c.object_id = object_id('tempdb..' + @P1) \
              order by c.column_id",
            &[name],
        )?;
        if rows.is_empty() {
            return Ok(None);
        }
        let columns = rows
            .into_iter()
            .filter_map(|r| {
                let value = |i: usize| r.get(i).cloned().flatten();
                let data_type = value(1)?;
                let length: i64 = value(2)?.parse().ok()?;
                let size = match data_type.as_str() {
                    "varchar" | "char" | "varbinary" | "binary" if length == -1 => "(max)".into(),
                    "nvarchar" | "nchar" if length == -1 => "(max)".into(),
                    "varchar" | "char" | "varbinary" | "binary" => format!("({length})"),
                    "nvarchar" | "nchar" => format!("({})", length / 2),
                    "decimal" | "numeric" => format!("({},{})", value(3)?, value(4)?),
                    _ => String::new(),
                };
                Some(CatalogColumn {
                    name: value(0)?,
                    data_type: format!("{data_type}{size}"),
                    nullable: value(5).is_some_and(|n| n == "1" || n.eq_ignore_ascii_case("true")),
                    comment: None,
                })
            })
            .collect();
        let object = CatalogObject {
            owner: "dbo".into(),
            name: name.to_owned(),
            object_type: "TABLE".into(),
        };
        Ok(Some((object, columns)))
    }

    fn first(row: Vec<Cell>) -> Option<String> {
        row.into_iter().next().flatten()
    }

    fn object(row: Vec<Cell>) -> Option<CatalogObject> {
        let mut row = row.into_iter();
        Some(CatalogObject {
            owner: row.next()??,
            name: row.next()??,
            object_type: row.next()??,
        })
    }
}

/// Answers `request` on Oracle, unqualified names in `context`'s current
/// schema (the session's when `None`).
fn fetch(
    conn: &Connection,
    request: CatalogRequest,
    context: Option<&NameContext>,
) -> Result<Answer, DbError> {
    let current = context.and_then(NameContext::current);
    Ok(match request {
        CatalogRequest::Schemas => {
            let rows =
                conn.query_as::<String>("select username from all_users order by username", &[])?;
            Answer::Schemas(rows.collect::<Result<_, _>>()?)
        }
        CatalogRequest::Objects { schema } => {
            let objects = objects(conn, schema.as_deref(), current)?;
            Answer::Objects(schema, objects)
        }
        CatalogRequest::Resolve { schema, name } => {
            let object = resolve(conn, schema.as_deref(), &name, current)?;
            Answer::Resolved(schema, name, object)
        }
        CatalogRequest::Columns { owner, name } => {
            let columns = columns(conn, &owner, &name)?;
            Answer::Columns(owner, name, columns)
        }
        CatalogRequest::Members { owner, name } => {
            let members = members(conn, &owner, &name)?;
            Answer::Members(owner, name, members)
        }
    })
}

/// `current`, or the session's current schema.
fn current_schema(conn: &Connection, current: Option<&str>) -> Result<String, DbError> {
    if let Some(current) = current {
        return Ok(current.to_owned());
    }
    Ok(conn.query_row_as(
        "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
        &[],
    )?)
}

/// Public synonyms read at most: a database has some ten thousand, Java's
/// (with a `/`) left out.
const PUBLIC_SYNONYMS: usize = 100_000;

/// Objects of a schema, all of them; for the current schema (`None`:
/// `current`) then public synonyms, so `dbms_` completes to the supplied
/// packages. One capped query of both dropped an arbitrary part of a large
/// schema (#161).
fn objects(
    conn: &Connection,
    schema: Option<&str>,
    current: Option<&str>,
) -> Result<Vec<CatalogObject>, DbError> {
    let owner = match schema {
        Some(s) => s.to_owned(),
        None => current_schema(conn, current)?,
    };
    let read = |sql: &str, params: &[&dyn pythia_db::oracle::sql_type::ToSql]| {
        let mut out = Vec::new();
        for row in conn.query_as::<(String, String, String)>(sql, params)? {
            let (owner, name, object_type) = row?;
            out.push(CatalogObject {
                owner,
                name,
                object_type,
            });
        }
        Ok::<_, DbError>(out)
    };
    let mut out = without_mview_tables(read(
        &format!(
            "select owner, object_name, object_type from all_objects \
              where owner = :1 and object_type in ({TYPES}) and generated = 'N' \
                and object_name not like 'BIN$%'"
        ),
        &[&owner],
    )?);
    if schema.is_none() {
        let synonyms = read(
            &pythia_db::compat::first_rows(
                "select 'PUBLIC', synonym_name, 'SYNONYM' from all_synonyms \
                  where owner = 'PUBLIC' and table_owner not in ('PUBLIC') \
                    and synonym_name not like '%/%'",
                PUBLIC_SYNONYMS,
            ),
            &[],
        )?;
        if synonyms.len() >= PUBLIC_SYNONYMS {
            tracing::warn!("completion: only the first {PUBLIC_SYNONYMS} public synonyms");
        }
        out.extend(synonyms);
    }
    Ok(out)
}

/// A materialized view's container table has the view's name: completion
/// offers the view once, not also the table.
fn without_mview_tables(mut objects: Vec<CatalogObject>) -> Vec<CatalogObject> {
    let mviews: std::collections::HashSet<(String, String)> = objects
        .iter()
        .filter(|o| o.object_type == "MATERIALIZED VIEW")
        .map(|o| (o.owner.clone(), o.name.clone()))
        .collect();
    objects.retain(|o| {
        o.object_type != "TABLE" || !mviews.contains(&(o.owner.clone(), o.name.clone()))
    });
    objects
}

/// Resolves a name the way Oracle does: own object, private synonym, public
/// synonym. Synonyms resolve to their target.
fn resolve(
    conn: &Connection,
    schema: Option<&str>,
    name: &str,
    current: Option<&str>,
) -> Result<Option<CatalogObject>, DbError> {
    let owner = match schema {
        Some(s) => s.to_owned(),
        None => current_schema(conn, current)?,
    };
    if let Some(object) = object_in(conn, &owner, name)? {
        return Ok(Some(object));
    }
    let synonym_owners: &[&str] = if schema.is_some() {
        &[]
    } else {
        &["", "PUBLIC"]
    };
    for syn_owner in synonym_owners {
        let syn_owner = if syn_owner.is_empty() {
            owner.as_str()
        } else {
            syn_owner
        };
        let target = conn.query_row_as::<(String, String)>(
            "select table_owner, table_name from all_synonyms where owner = :1 and synonym_name = :2 and db_link is null",
            &[&syn_owner, &name],
        );
        match target {
            Ok((t_owner, t_name)) => return object_in(conn, &t_owner, &t_name),
            Err(e) if e.kind() == pythia_db::oracle::ErrorKind::NoDataFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

fn object_in(conn: &Connection, owner: &str, name: &str) -> Result<Option<CatalogObject>, DbError> {
    // Prefer the spec over bodies and tables over their indexes etc.
    let sql = format!(
        "select owner, object_name, object_type from all_objects \
          where owner = :1 and object_name = :2 and object_type in ({TYPES}) \
          order by decode(object_type, 'TABLE', 1, 'VIEW', 2, 'PACKAGE', 3, 'TYPE', 4, 5)"
    );
    let sql = pythia_db::compat::first_rows(&sql, 1);
    match conn.query_row_as::<(String, String, String)>(&sql, &[&owner, &name]) {
        Ok((owner, name, object_type)) => Ok(Some(CatalogObject {
            owner,
            name,
            object_type,
        })),
        Err(e) if e.kind() == pythia_db::oracle::ErrorKind::NoDataFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Table/view columns, or the attributes of an object type.
fn columns(conn: &Connection, owner: &str, name: &str) -> Result<Vec<CatalogColumn>, DbError> {
    let rows = conn.query_as::<(String, String, String, Option<String>)>(
        "select c.column_name, \
                case when c.data_type in ('VARCHAR2', 'NVARCHAR2', 'CHAR', 'NCHAR', 'RAW') \
                       then c.data_type || '(' || decode(c.char_used, 'C', c.char_length || ' CHAR', c.data_length) || ')' \
                     when c.data_type = 'NUMBER' and c.data_precision is not null \
                       then 'NUMBER(' || c.data_precision || nvl2(nullif(c.data_scale, 0), ',' || c.data_scale, '') || ')' \
                     else c.data_type end, \
                c.nullable, cc.comments \
           from all_tab_columns c \
           left join all_col_comments cc \
             on cc.owner = c.owner and cc.table_name = c.table_name and cc.column_name = c.column_name \
          where c.owner = :1 and c.table_name = :2 order by c.column_id",
        &[&owner, &name],
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (name, data_type, nullable, comment) = row?;
        out.push(CatalogColumn {
            name,
            data_type,
            nullable: nullable == "Y",
            comment,
        });
    }
    if out.is_empty() {
        let rows = conn.query_as::<(String, String)>(
            "select attr_name, attr_type_name from all_type_attrs where owner = :1 and type_name = :2 order by attr_no",
            &[&owner, &name],
        )?;
        for row in rows {
            let (name, data_type) = row?;
            out.push(CatalogColumn {
                name,
                data_type,
                nullable: true,
                comment: None,
            });
        }
    }
    Ok(out)
}

/// Subprograms of a package or type, or a standalone procedure/function
/// as its single member, with parameters from ALL_ARGUMENTS.
fn members(conn: &Connection, owner: &str, name: &str) -> Result<Vec<CatalogMember>, DbError> {
    // The type is NULL in the row 11g has for a parameterless subprogram.
    #[allow(clippy::type_complexity)]
    let rows = conn.query_as::<(
        String,
        Option<String>,
        u32,
        Option<String>,
        String,
        Option<String>,
        String,
        u32,
    )>(
        &format!(
            "select a.object_name, a.overload, a.position, a.argument_name, a.in_out, \
                {}, nvl(a.defaulted, 'N'), a.subprogram_id \
           from all_arguments a \
          where a.owner = :1 and a.data_level = 0 \
            and ((a.package_name = :2) or (a.package_name is null and a.object_name = :2)) \
          order by a.subprogram_id, a.overload, a.position",
            pythia_db::compat::ARGUMENT_TYPE
        ),
        &[&owner, &name],
    )?;
    let mut out: Vec<CatalogMember> = Vec::new();
    let mut current: Option<(u32, Option<String>)> = None;
    for row in rows {
        let (proc, overload, position, arg, in_out, data_type, defaulted, subprogram) = row?;
        let data_type = data_type.unwrap_or_default();
        let key = (subprogram, overload.clone());
        if current.as_ref() != Some(&key) {
            current = Some(key);
            out.push(CatalogMember {
                name: proc,
                kind: MemberKind::Procedure,
                params: Vec::new(),
                return_type: None,
                overload: overload.and_then(|o| o.parse().ok()),
            });
        }
        let member = out.last_mut().expect("pushed above");
        match (position, arg) {
            (0, _) => {
                member.kind = MemberKind::Function;
                member.return_type = Some(data_type);
            }
            (_, Some(arg)) => member.params.push(CatalogParam {
                name: arg,
                mode: match in_out.as_str() {
                    "OUT" => ParamMode::Out,
                    "IN/OUT" => ParamMode::InOut,
                    _ => ParamMode::In,
                },
                data_type,
                has_default: defaulted == "Y",
            }),
            // A parameterless subprogram has a single row without a name.
            (_, None) => {}
        }
    }
    // Package subprograms without any argument rows don't appear in
    // ALL_ARGUMENTS; pick them up from ALL_PROCEDURES.
    let rows = conn.query_as::<(String, Option<String>)>(
        "select procedure_name, overload from all_procedures \
          where owner = :1 and object_name = :2 and procedure_name is not null",
        &[&owner, &name],
    )?;
    for row in rows {
        let (proc, overload) = row?;
        let overload: Option<u32> = overload.and_then(|o| o.parse().ok());
        if !out.iter().any(|m| m.name == proc && m.overload == overload) {
            out.push(CatalogMember {
                name: proc,
                kind: MemberKind::Procedure,
                params: Vec::new(),
                return_type: None,
                overload,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use pythia_lsp_core::{Analysis, Catalog};

    use super::*;

    /// Runs `f` until the engine stops asking for dictionary data, answering
    /// every request from the live database.
    fn settle<T>(
        conn: &Connection,
        cache: &mut CatalogCache,
        mut f: impl FnMut(&dyn Catalog) -> (T, Vec<CatalogRequest>),
    ) -> T {
        for _ in 0..8 {
            let (value, missing) = f(cache);
            if missing.is_empty() {
                return value;
            }
            for request in missing {
                fetch(conn, request, None)
                    .expect("catalog query")
                    .apply(cache);
            }
        }
        panic!("catalog requests did not settle");
    }

    #[test]
    fn materialized_views_once() {
        let obj = |name: &str, object_type: &str| CatalogObject {
            owner: "IFSAPP".into(),
            name: name.into(),
            object_type: object_type.into(),
        };
        let objects = without_mview_tables(vec![
            obj("MXN_ACTIVITY_MV", "TABLE"),
            obj("MXN_ACTIVITY_MV", "MATERIALIZED VIEW"),
            obj("MXN_ITEM_TAB", "TABLE"),
            obj("MXN_ITEM", "VIEW"),
        ]);
        assert_eq!(
            objects
                .iter()
                .map(|o| (o.name.as_str(), o.object_type.as_str()))
                .collect::<Vec<_>>(),
            [
                ("MXN_ACTIVITY_MV", "MATERIALIZED VIEW"),
                ("MXN_ITEM_TAB", "TABLE"),
                ("MXN_ITEM", "VIEW"),
            ]
        );
    }

    /// The catalog of the current schema: its objects, a materialized view
    /// once, and the public synonyms after them (#161).
    #[test]
    fn schema_objects_and_public_synonyms() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            eprintln!("PYTHIA_TEST_DB not set, skipping");
            return;
        };
        crate::test_db::demo_schema();
        crate::test_db::as_dba(&["grant create materialized view to {user}"]);
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let _ = conn.execute("drop materialized view pcat_mv", &[]);
        conn.execute(
            "create materialized view pcat_mv as select department_id, count(*) n \
               from demo_employees group by department_id",
            &[],
        )
        .unwrap();
        let objects = objects(&conn, None, None).unwrap();
        conn.execute("drop materialized view pcat_mv", &[]).unwrap();
        let kinds = |name: &str| -> Vec<&str> {
            objects
                .iter()
                .filter(|o| o.name == name)
                .map(|o| o.object_type.as_str())
                .collect()
        };
        assert_eq!(kinds("PCAT_MV"), ["MATERIALIZED VIEW"]);
        assert_eq!(kinds("DEMO_EMPLOYEES"), ["TABLE"]);
        let first_public = objects.iter().position(|o| o.owner == "PUBLIC");
        assert!(kinds("DBMS_OUTPUT").contains(&"SYNONYM"));
        assert!(
            first_public.is_some_and(|p| objects[p..].iter().all(|o| o.owner == "PUBLIC")),
            "the schema's objects first"
        );
    }

    #[test]
    fn completion_against_live_database() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            eprintln!("PYTHIA_TEST_DB not set, skipping");
            return;
        };
        crate::test_db::demo_schema();
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let mut cache = CatalogCache::new(current_schema(&conn, None).unwrap());

        // Columns after an alias (the table is resolved and described).
        let src = "select e. from demo_employees e";
        let analysis = Analysis::new(src);
        let labels = settle(&conn, &mut cache, |cat| {
            let c = analysis.complete(src.find("e.").unwrap() + 2, cat);
            (
                c.items
                    .iter()
                    .map(|i| i.label.to_uppercase())
                    .collect::<Vec<_>>(),
                c.missing,
            )
        });
        assert!(
            labels.contains(&"SALARY".to_owned()) && labels.contains(&"EMPLOYEE_ID".to_owned()),
            "{labels:?}"
        );

        // Package members, with parameters from ALL_ARGUMENTS.
        let src = "begin demo_pkg.ra";
        let analysis = Analysis::new(src);
        let items = settle(&conn, &mut cache, |cat| {
            let c = analysis.complete(src.len(), cat);
            (c.items, c.missing)
        });
        let raise = items
            .iter()
            .find(|i| i.label.eq_ignore_ascii_case("raise_salary"))
            .expect("member");
        assert!(
            raise.detail.to_lowercase().contains("p_employee_id"),
            "{raise:?}"
        );

        // Signature help inside the call.
        let src = "begin demo_pkg.raise_salary(1, ";
        let analysis = Analysis::new(src);
        let help = settle(&conn, &mut cache, |cat| {
            let h = analysis.signature_help(src.len(), cat).expect("signature");
            let missing = h.missing.clone();
            (h, missing)
        });
        assert_eq!(help.active_param, 1);
        assert!(
            help.signatures[0]
                .label
                .to_lowercase()
                .contains("p_percent"),
            "{:?}",
            help.signatures
        );

        // Public synonyms for supplied packages.
        let src = "begin dbms_outp";
        let analysis = Analysis::new(src);
        let labels = settle(&conn, &mut cache, |cat| {
            let c = analysis.complete(src.len(), cat);
            (
                c.items
                    .iter()
                    .map(|i| i.label.to_uppercase())
                    .collect::<Vec<_>>(),
                c.missing,
            )
        });
        assert!(labels.contains(&"DBMS_OUTPUT".to_owned()), "{labels:?}");

        // Go to definition resolves a table through the dictionary.
        let src = "select * from demo_departments";
        let analysis = Analysis::new(src);
        let target = settle(&conn, &mut cache, |cat| {
            let d = analysis.definition(src.len() - 3, cat);
            (d.target, d.missing)
        });
        assert!(
            matches!(&target, Some(pythia_lsp_core::Definition::Object { name, object_type, .. })
                if name == "DEMO_DEPARTMENTS" && object_type == "TABLE"),
            "{target:?}"
        );
    }
}

/// Completion with the dictionaries of MySQL, PostgreSQL and SQL Server
/// (each skipped unless `PYTHIA_TEST_MYSQL`, `PYTHIA_TEST_POSTGRES` or
/// `PYTHIA_TEST_MSSQL` is set to `user/password@host:port/database`).
#[cfg(test)]
mod dictionary_live {
    use std::sync::mpsc;
    use std::time::Duration;

    use pythia_db::{ConnectParams, DbEvent, NameContext, Session};
    use pythia_lsp_core::{Analysis, CatalogCache, CatalogRequest, NameStyle};

    use super::*;

    struct Live {
        session: Session,
        wake: mpsc::Receiver<()>,
        dialect: Dialect,
    }

    impl Live {
        fn next(&mut self) -> DbEvent {
            loop {
                if let Some(event) = self.session.try_recv() {
                    return event;
                }
                self.wake
                    .recv_timeout(Duration::from_secs(30))
                    .expect("timed out");
            }
        }

        fn answer(&mut self, request: CatalogRequest) -> Answer {
            self.answer_in(request, None)
        }

        /// The answer of `request` for names in `context`.
        fn answer_in(&mut self, request: CatalogRequest, context: Option<NameContext>) -> Answer {
            self.session
                .send(super::request(7, request, self.dialect, context));
            loop {
                if let DbEvent::TaskDone { result, .. } = self.next() {
                    return *result
                        .expect("catalog query")
                        .downcast::<Answer>()
                        .expect("an answer");
                }
            }
        }

        fn run(&mut self, sql: &str) {
            if let Err(error) = self.try_run(sql) {
                panic!("{sql}: {error}");
            }
        }

        /// The first value of query `sql`.
        fn first(&mut self, sql: &str) -> Option<String> {
            self.session.send(DbRequest::Execute {
                id: 2,
                sql: sql.into(),
                max_rows: 10,
                binds: Vec::new(),
            });
            loop {
                match self.next() {
                    DbEvent::Rows { id: 2, rows, .. } => {
                        return rows.into_iter().next()?.into_iter().next()?;
                    }
                    DbEvent::Failed { id: 2, error, .. } => panic!("{sql}: {error}"),
                    _ => {}
                }
            }
        }

        fn try_run(&mut self, sql: &str) -> Result<(), pythia_db::DbError> {
            self.session.send(DbRequest::Execute {
                id: 1,
                sql: sql.into(),
                max_rows: 10,
                binds: Vec::new(),
            });
            loop {
                match self.next() {
                    DbEvent::Executed { .. } => return Ok(()),
                    DbEvent::Failed { error, .. } => return Err(error),
                    _ => {}
                }
            }
        }
    }

    /// The labels completed at `¦` in `src` with `cache`, which keeps what
    /// `live` answered.
    fn labels_at(live: &mut Live, cache: &mut CatalogCache, src: &str) -> Vec<String> {
        let offset = src.find('¦').expect("a caret");
        let src = src.replacen('¦', "", 1);
        let analysis = Analysis::new_in(&src, live.dialect.syntax());
        for _ in 0..8 {
            let c = analysis.complete(offset, cache);
            if c.missing.is_empty() {
                return c.items.into_iter().map(|i| i.label).collect();
            }
            for request in c.missing {
                live.answer(request).apply(cache);
            }
        }
        panic!("{src}: still missing dictionary data");
    }

    /// Connects to the database of `env` (`user/password@connect string`).
    fn connect_live(env: &str, kind: DbKind) -> Option<(Live, String)> {
        let spec = std::env::var(env).ok()?;
        let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
        let (username, password) = credentials.split_once('/').unwrap();
        Some(connect_with(
            kind,
            username,
            password,
            connect_string,
            Vec::new(),
        ))
    }

    fn connect_with(
        kind: DbKind,
        username: &str,
        password: &str,
        connect_string: &str,
        init_sql: Vec<String>,
    ) -> (Live, String) {
        let (tx, wake) = mpsc::channel();
        let session = Session::connect(
            ConnectParams {
                kind,
                username: username.into(),
                password: password.into(),
                connect_string: connect_string.into(),
                privilege: Default::default(),
                init_sql,
                ssh: None,
            },
            move || {
                let _ = tx.send(());
            },
        );
        let mut live = Live {
            session,
            wake,
            dialect: kind.into(),
        };
        let DbEvent::Connected(server) = live.next() else {
            panic!("{connect_string}: not connected")
        };
        (live, server.current_schema)
    }

    /// Columns after the alias of a table in another schema, once that
    /// schema's objects were completed after `r51_s.` (#282): they are
    /// kept as stored (`r51_t`), and the table, typed without quotes and
    /// looked up as `R51_T`, wasn't found among them and was taken for one
    /// that doesn't exist. On SQLite (an attached file), and on SQL
    /// Server, PostgreSQL and MySQL (a database for a schema) when their
    /// test databases are set.
    #[test]
    fn columns_after_a_schema_was_completed() {
        let file =
            std::env::temp_dir().join(format!("pythia-catalog-{}-r51.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        rusqlite::Connection::open(&file)
            .unwrap()
            .execute_batch("create table r51_t (id integer primary key, note text);")
            .unwrap();
        let attach = vec![format!("attach '{}' as r51_s", file.display())];
        let mut lives = vec![(
            connect_with(DbKind::Sqlite, "", "", ":memory:", attach),
            NameStyle::AsWritten,
            "",
        )];
        let servers = [
            (
                "PYTHIA_TEST_MSSQL",
                DbKind::SqlServer,
                NameStyle::Brackets,
                "create schema r51_s",
                "drop table if exists r51_s.r51_t; drop schema if exists r51_s",
            ),
            (
                "PYTHIA_TEST_POSTGRES",
                DbKind::Postgres,
                NameStyle::Lower,
                "create schema r51_s",
                "drop schema if exists r51_s cascade",
            ),
            (
                "PYTHIA_TEST_MYSQL_DBA",
                DbKind::MySql,
                NameStyle::Backticks,
                "create database r51_s",
                "drop database if exists r51_s",
            ),
        ];
        for (env, kind, names, create, drop) in servers {
            let Some((mut live, schema)) = connect_live(env, kind) else {
                continue;
            };
            for sql in drop.split("; ") {
                live.run(sql);
            }
            live.run(create);
            live.run("create table r51_s.r51_t (id int primary key, note varchar(20))");
            live.session.send(DbRequest::Commit);
            lives.push(((live, schema), names, drop));
        }
        for ((live, schema), names, drop) in &mut lives {
            let mut cache = CatalogCache::new(schema.clone()).with_names(*names);
            let tables = labels_at(live, &mut cache, "select * from r51_s.¦");
            assert!(tables.contains(&"r51_t".to_owned()), "{tables:?}");
            for src in [
                "select t.¦ from r51_s.r51_t t",
                "select * from r51_s.r51_t where r51_t.¦",
            ] {
                let columns = labels_at(live, &mut cache, src);
                assert!(
                    columns.contains(&"note".to_owned()),
                    "{}: {src}: {columns:?}",
                    live.dialect.name()
                );
            }
            for sql in drop.split("; ").filter(|s| !s.is_empty()) {
                live.run(sql);
            }
            live.session.send(DbRequest::Commit);
        }
        drop(lives);
        let _ = std::fs::remove_file(&file);
    }

    /// PostgreSQL: lookups in a SQL window's session leave its open
    /// transaction as it was (#320), also one that fails, which would have
    /// aborted it: the window's pending row stays, and its statements run
    /// (instead of ERROR 25P02).
    #[test]
    fn postgres_lookups_keep_the_transaction() {
        let Some((mut live, _)) = connect_live("PYTHIA_TEST_POSTGRES", DbKind::Postgres) else {
            return;
        };
        live.run("drop table if exists r75_g");
        live.run("create table r75_g (id int)");
        live.session.send(DbRequest::Commit);
        live.run("create temp table r75_gt (id int, note text)");
        live.run("insert into r75_g values (1)");
        let run = |live: &mut Live, task: pythia_db::RunFn| {
            live.session.send(DbRequest::Run { id: 9, task });
            loop {
                if let DbEvent::TaskDone { id: 9, result } = live.next() {
                    return result;
                }
            }
        };
        let temporary = run(
            &mut live,
            Box::new(|client| {
                let answer = dictionary::own(client, OwnRequest::Temporary)?;
                Ok(Box::new(answer) as Box<dyn std::any::Any + Send>)
            }),
        );
        match temporary.map(|any| any.downcast::<OwnAnswer>()) {
            Ok(Ok(answer)) => assert!(
                matches!(&*answer, OwnAnswer::Temporary(t) if t.iter().any(|o| o.name == "r75_gt"))
            ),
            _ => panic!("no temporary tables"),
        }
        let failed = run(
            &mut live,
            Box::new(|client| {
                dictionary::guarded(client, |client| client.query("select 1 / 0", &[]))?;
                Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
            }),
        );
        assert!(failed.is_err());
        assert_eq!(
            live.first("select count(*) from r75_g").as_deref(),
            Some("1")
        );
        live.run("insert into r75_g values (2)");
        live.session.send(DbRequest::Rollback);
        assert_eq!(
            live.first("select count(*) from r75_g").as_deref(),
            Some("0")
        );
        live.run("drop table r75_g");
        live.session.send(DbRequest::Commit);
    }

    /// SQL Server: lookups in a window's database read that database's
    /// catalog views (#320), where its `use` made the window's names be:
    /// master's schemas haven't the test database's.
    #[test]
    fn sqlserver_names_of_another_database() {
        let Some((mut live, _)) = connect_live("PYTHIA_TEST_MSSQL", DbKind::SqlServer) else {
            return;
        };
        live.run("drop schema if exists r75_d");
        live.run("create schema r75_d");
        let mut schemas = |context| match live.answer_in(CatalogRequest::Schemas, context) {
            Answer::Schemas(schemas) => schemas,
            _ => panic!("not schemas"),
        };
        let own = schemas(None);
        assert!(own.iter().any(|s| s == "r75_d"), "{own:?}");
        let master = NameContext {
            database: "master".into(),
            schemas: vec!["guest".into()],
            ..NameContext::default()
        };
        let in_master = schemas(Some(master));
        assert!(
            !in_master.iter().any(|s| s == "r75_d") && in_master.iter().any(|s| s == "guest"),
            "{in_master:?}"
        );
        live.run("drop schema r75_d");
    }

    /// Completes the columns of `cat_orders`, written as `table`, after its
    /// alias, created by `create` on the database of `env`; returns
    /// `(label, inserted text)`.
    fn complete_columns(
        env: &str,
        kind: DbKind,
        names: NameStyle,
        create: &str,
        table: &str,
    ) -> Option<Vec<(String, String)>> {
        let (mut live, server) = connect(env, kind)?;
        live.run("drop table if exists cat_orders");
        live.run(create);
        let items = complete_on(&mut live, server.current_schema, names, table);
        live.run("drop table cat_orders");
        Some(items)
    }

    /// A session on the database of `env`, connected.
    fn connect(env: &str, kind: DbKind) -> Option<(Live, pythia_db::ServerInfo)> {
        let spec = std::env::var(env).ok()?;
        let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
        let (username, password) = credentials.split_once('/').unwrap();
        let (tx, wake) = mpsc::channel();
        let session = Session::connect(
            ConnectParams {
                kind,
                username: username.into(),
                password: password.into(),
                connect_string: connect_string.into(),
                privilege: Default::default(),
                init_sql: Vec::new(),
                ssh: None,
            },
            move || {
                let _ = tx.send(());
            },
        );
        let mut live = Live {
            session,
            wake,
            dialect: kind.into(),
        };
        let DbEvent::Connected(server) = live.next() else {
            panic!("not connected")
        };
        Some((live, server))
    }

    /// Completes the columns after the alias of `table` in `select o.\n  from
    /// {table} o` on `live`; returns `(label, inserted text)`.
    fn complete_on(
        live: &mut Live,
        schema: String,
        names: NameStyle,
        table: &str,
    ) -> Vec<(String, String)> {
        // Code assistance looks unquoted names up in uppercase, quoted ones
        // as written, lexed as the database reads them (#208).
        let src = format!("select o.\n  from {table} o");
        complete_at(live, schema, names, &src, 9)
    }

    /// Completes `src` at `at` on `live`; returns `(label, inserted text)`.
    fn complete_at(
        live: &mut Live,
        schema: String,
        names: NameStyle,
        src: &str,
        at: usize,
    ) -> Vec<(String, String)> {
        let mut cache = CatalogCache::new(schema).with_names(names);
        let analysis = Analysis::new_in(src, live.dialect.syntax());
        let mut items = Vec::new();
        for _ in 0..8 {
            let c = analysis.complete(at, &cache);
            if c.missing.is_empty() {
                items = c.items;
                break;
            }
            for request in c.missing {
                live.answer(request).apply(&mut cache);
            }
        }
        items
            .into_iter()
            .map(|i| (i.label, i.insert_text))
            .collect()
    }

    fn has(items: &[(String, String)], label: &str, text: &str) -> bool {
        items.iter().any(|(l, t)| l == label && t == text)
    }

    #[test]
    fn mysql_columns_after_an_alias() {
        for table in ["CAT_ORDERS", "`cat_orders`"] {
            let Some(items) = complete_columns(
                "PYTHIA_TEST_MYSQL",
                DbKind::MySql,
                NameStyle::Backticks,
                "create table cat_orders (id int primary key, Customer varchar(30), `order` int)",
                table,
            ) else {
                return;
            };
            // Names as stored; backticks only where MySQL needs them.
            assert!(has(&items, "Customer", "Customer"), "{table}: {items:?}");
            assert!(has(&items, "order", "`order`"), "{table}: {items:?}");
        }
    }

    /// MariaDB's sequences complete as sequences (#270): in an expression,
    /// not after FROM, where they were offered as tables.
    #[test]
    fn mariadb_sequences_complete_as_sequences() {
        let Some((mut live, server)) = connect("PYTHIA_TEST_MYSQL", DbKind::MySql) else {
            return;
        };
        if !server.version.contains("MariaDB") {
            return;
        }
        live.run("drop sequence if exists r53_cat_seq");
        live.run("drop table if exists r53_cat_tab");
        live.run("create sequence r53_cat_seq");
        live.run("create table r53_cat_tab (id int)");
        let mut labels = |src: &str| -> Vec<String> {
            let items = complete_at(
                &mut live,
                server.current_schema.clone(),
                NameStyle::Backticks,
                src,
                src.len(),
            );
            items
                .into_iter()
                .map(|(label, _)| label)
                .filter(|l| l.starts_with("r53_cat"))
                .collect()
        };
        assert_eq!(labels("select * from r53_cat"), ["r53_cat_tab"]);
        assert_eq!(labels("select r53_cat"), ["r53_cat_seq"]);
        live.run("drop sequence r53_cat_seq");
        live.run("drop table r53_cat_tab");
    }

    #[test]
    fn postgres_columns_after_an_alias() {
        let Some(items) = complete_columns(
            "PYTHIA_TEST_POSTGRES",
            DbKind::Postgres,
            NameStyle::Lower,
            "create table cat_orders (id int primary key, \"Customer\" varchar(30), \"order\" int)",
            "CAT_ORDERS",
        ) else {
            return;
        };
        // Lowercase names as typed; quotes where PostgreSQL needs them.
        assert!(has(&items, "id", "id"), "{items:?}");
        assert!(has(&items, "\"Customer\"", "\"Customer\""), "{items:?}");
        assert!(has(&items, "\"order\"", "\"order\""), "{items:?}");
    }

    #[test]
    fn sqlserver_columns_after_an_alias() {
        for table in ["CAT_ORDERS", "[cat_orders]", "[dbo].[cat_orders]"] {
            let Some(items) = complete_columns(
                "PYTHIA_TEST_MSSQL",
                DbKind::SqlServer,
                NameStyle::Brackets,
                "create table cat_orders (id int primary key, Customer varchar(30), [order] int)",
                table,
            ) else {
                return;
            };
            // Names as stored; brackets only where SQL Server needs them.
            assert!(has(&items, "Customer", "Customer"), "{table}: {items:?}");
            assert!(has(&items, "order", "[order]"), "{table}: {items:?}");
        }
    }

    /// SQLite looks an unqualified name up in `temp`, `main`, then the
    /// attached databases in the order they were attached; completion
    /// looked in `temp` and `main` only, and offered nothing for a table of
    /// an attached file (#236).
    #[test]
    fn sqlite_tables_of_attached_databases() {
        let dir = std::env::temp_dir();
        let file =
            |name: &str| dir.join(format!("pythia-catalog-{}-{name}.db", std::process::id()));
        let (main, other) = (file("main"), file("other"));
        for (path, sql) in [
            (
                &main,
                "create table accounts (id integer primary key, fullname text);",
            ),
            (
                &other,
                "create table accounts (id integer primary key, firstname text);\n\
                 create table only_other (id integer primary key, note text);",
            ),
        ] {
            let _ = std::fs::remove_file(path);
            rusqlite::Connection::open(path)
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
        let (tx, wake) = mpsc::channel();
        let session = Session::connect(
            ConnectParams {
                kind: DbKind::Sqlite,
                username: String::new(),
                password: String::new(),
                connect_string: main.display().to_string(),
                privilege: Default::default(),
                init_sql: vec![format!("attach '{}' as other", other.display())],
                ssh: None,
            },
            move || {
                let _ = tx.send(());
            },
        );
        let mut live = Live {
            session,
            wake,
            dialect: Dialect::SQLITE,
        };
        let DbEvent::Connected(server) = live.next() else {
            panic!("not connected")
        };
        let mut complete = |table: &str| {
            let items = complete_on(
                &mut live,
                server.current_schema.clone(),
                NameStyle::AsWritten,
                table,
            );
            let mut labels: Vec<String> = items.into_iter().map(|(label, _)| label).collect();
            labels.sort();
            labels
        };
        assert_eq!(complete("only_other"), ["id", "note"]);
        // `main` comes before the attached ones.
        assert_eq!(complete("accounts"), ["fullname", "id"]);
        assert_eq!(complete("other.accounts"), ["firstname", "id"]);
        drop(live);
        let _ = std::fs::remove_file(&main);
        let _ = std::fs::remove_file(&other);
    }
}
