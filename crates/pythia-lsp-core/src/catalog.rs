//! Access to database metadata without ever blocking.
//!
//! The engine asks a [`Catalog`] for dictionary data. A lookup either answers
//! from what is cached ([`Lookup::Ready`]) or says the data isn't there yet
//! ([`Lookup::Pending`]). Every pending lookup is reported back to the caller
//! as a [`CatalogRequest`] in the result's `missing` list; the app fetches
//! those asynchronously, stores them (e.g. in a [`CatalogCache`]) and runs the
//! request again to get the complete answer.

use std::cell::RefCell;
use std::collections::HashMap;

pub use pythia_plsql::ast::ParamMode;

/// Result of a non-blocking metadata lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<T> {
    Ready(T),
    /// Not cached yet; the engine reports a [`CatalogRequest`] for it.
    Pending,
}

/// A schema object. `object_type` uses dictionary names (`TABLE`,
/// `PACKAGE`, `VIEW`, ...); synonyms are reported resolved to their target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CatalogObject {
    pub owner: String,
    pub name: String,
    pub object_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemberKind {
    Procedure,
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogParam {
    pub name: String,
    pub mode: ParamMode,
    pub data_type: String,
    pub has_default: bool,
}

/// A subprogram of a package or object type — or a standalone procedure /
/// function, which has exactly one member: itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogMember {
    pub name: String,
    pub kind: MemberKind,
    pub params: Vec<CatalogParam>,
    pub return_type: Option<String>,
    pub overload: Option<u32>,
}

/// Dictionary data the engine needed but the catalog didn't have.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CatalogRequest {
    /// All schema names (`ALL_USERS`).
    Schemas,
    /// Objects of a schema; `None` = current schema.
    Objects { schema: Option<String> },
    /// Resolve a name like Oracle does (schema object, private synonym,
    /// public synonym). `schema` is set for qualified names.
    Resolve {
        schema: Option<String>,
        name: String,
    },
    /// Columns of a table/view, attributes of an object type.
    Columns { owner: String, name: String },
    /// Subprograms of a package/type, or the standalone subprogram itself.
    Members { owner: String, name: String },
}

/// Read access to cached dictionary data. All names are as stored in the
/// dictionary (unquoted names uppercase).
/// How a database writes the names in its dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NameStyle {
    /// Oracle: unquoted names are stored in uppercase; any other name is
    /// written in double quotes.
    #[default]
    Upper,
    /// PostgreSQL: unquoted names are stored in lowercase; any other name
    /// is written in double quotes.
    Lower,
    /// MySQL: names are stored as written and compared without case;
    /// backticks quote names that need it.
    Backticks,
    /// SQL Server: names are stored as written and compared without case;
    /// square brackets quote names that need it.
    Brackets,
    /// SQLite: names are stored as written and compared without case;
    /// double quotes quote names that need it.
    AsWritten,
}

pub trait Catalog {
    /// Schema unqualified names are looked up in.
    fn current_schema(&self) -> &str;

    /// How names are written; Oracle's way unless the database differs.
    fn names(&self) -> NameStyle {
        NameStyle::Upper
    }

    fn schemas(&self) -> Lookup<Vec<String>>;

    /// Objects of `schema` (`None` = current schema). May return everything;
    /// the engine filters by what the user typed.
    fn objects(&self, schema: Option<&str>) -> Lookup<Vec<CatalogObject>>;

    /// Resolves a name. `Ready(None)` means it definitely isn't an object
    /// (in that schema / via synonyms).
    fn resolve(&self, schema: Option<&str>, name: &str) -> Lookup<Option<CatalogObject>>;

    /// Columns of a table or view; attributes of an object type.
    fn columns(&self, owner: &str, name: &str) -> Lookup<Vec<CatalogColumn>>;

    /// Subprograms of a package or type; a standalone procedure or function
    /// returns itself as the only member.
    fn members(&self, owner: &str, name: &str) -> Lookup<Vec<CatalogMember>>;
}

/// A catalog that knows nothing and never asks for anything (offline use).
#[derive(Debug, Default, Clone)]
pub struct EmptyCatalog;

impl Catalog for EmptyCatalog {
    fn current_schema(&self) -> &str {
        ""
    }
    fn schemas(&self) -> Lookup<Vec<String>> {
        Lookup::Ready(Vec::new())
    }
    fn objects(&self, _: Option<&str>) -> Lookup<Vec<CatalogObject>> {
        Lookup::Ready(Vec::new())
    }
    fn resolve(&self, _: Option<&str>, _: &str) -> Lookup<Option<CatalogObject>> {
        Lookup::Ready(None)
    }
    fn columns(&self, _: &str, _: &str) -> Lookup<Vec<CatalogColumn>> {
        Lookup::Ready(Vec::new())
    }
    fn members(&self, _: &str, _: &str) -> Lookup<Vec<CatalogMember>> {
        Lookup::Ready(Vec::new())
    }
}

/// Ready-made in-memory cache implementing [`Catalog`]. Anything not stored
/// yet is `Pending`; the app answers [`CatalogRequest`]s by calling the
/// matching `set_*` method.
#[derive(Debug, Default, Clone)]
pub struct CatalogCache {
    current_schema: String,
    names: NameStyle,
    schemas: Option<Vec<String>>,
    objects: HashMap<String, Vec<CatalogObject>>,
    resolved: HashMap<(Option<String>, String), Option<CatalogObject>>,
    columns: HashMap<(String, String), Vec<CatalogColumn>>,
    members: HashMap<(String, String), Vec<CatalogMember>>,
}

impl CatalogCache {
    pub fn new(current_schema: impl Into<String>) -> Self {
        Self {
            current_schema: current_schema.into(),
            ..Self::default()
        }
    }

    /// For a database that writes names differently than Oracle.
    pub fn with_names(mut self, names: NameStyle) -> Self {
        self.names = names;
        self
    }

    pub fn set_current_schema(&mut self, schema: impl Into<String>) {
        self.current_schema = schema.into();
    }

    pub fn set_schemas(&mut self, schemas: Vec<String>) {
        self.schemas = Some(schemas);
    }

    /// Objects of a schema (`None` = current schema).
    pub fn set_objects(&mut self, schema: Option<&str>, objects: Vec<CatalogObject>) {
        let schema = schema.unwrap_or(&self.current_schema).to_owned();
        self.objects.insert(schema, objects);
    }

    pub fn set_resolved(
        &mut self,
        schema: Option<&str>,
        name: &str,
        object: Option<CatalogObject>,
    ) {
        self.resolved
            .insert((schema.map(str::to_owned), name.to_owned()), object);
    }

    pub fn set_columns(&mut self, owner: &str, name: &str, columns: Vec<CatalogColumn>) {
        self.columns
            .insert((owner.to_owned(), name.to_owned()), columns);
    }

    pub fn set_members(&mut self, owner: &str, name: &str, members: Vec<CatalogMember>) {
        self.members
            .insert((owner.to_owned(), name.to_owned()), members);
    }

    /// Drops everything (e.g. after DDL), keeping the current schema.
    pub fn clear(&mut self) {
        *self = Self::new(std::mem::take(&mut self.current_schema)).with_names(self.names);
    }

    /// Forgets one object's details (after it was recompiled or altered).
    pub fn invalidate(&mut self, owner: &str, name: &str) {
        let key = (owner.to_owned(), name.to_owned());
        self.columns.remove(&key);
        self.members.remove(&key);
    }
}

impl Catalog for CatalogCache {
    fn current_schema(&self) -> &str {
        &self.current_schema
    }

    fn names(&self) -> NameStyle {
        self.names
    }

    fn schemas(&self) -> Lookup<Vec<String>> {
        self.schemas.clone().map_or(Lookup::Pending, Lookup::Ready)
    }

    fn objects(&self, schema: Option<&str>) -> Lookup<Vec<CatalogObject>> {
        let schema = schema.unwrap_or(&self.current_schema);
        self.objects
            .get(schema)
            .cloned()
            .map_or(Lookup::Pending, Lookup::Ready)
    }

    fn resolve(&self, schema: Option<&str>, name: &str) -> Lookup<Option<CatalogObject>> {
        if let Some(hit) = self
            .resolved
            .get(&(schema.map(str::to_owned), name.to_owned()))
        {
            return Lookup::Ready(hit.clone());
        }
        // An object of the schema itself resolves without a round trip.
        let in_schema = schema.unwrap_or(&self.current_schema);
        if let Some(objects) = self.objects.get(in_schema)
            && let Some(o) = objects
                .iter()
                .find(|o| o.name == name && o.object_type != "SYNONYM")
        {
            return Lookup::Ready(Some(o.clone()));
        }
        // A qualified name in a fully known schema that isn't there doesn't exist.
        if schema.is_some() && self.objects.contains_key(in_schema) {
            return Lookup::Ready(None);
        }
        Lookup::Pending
    }

    fn columns(&self, owner: &str, name: &str) -> Lookup<Vec<CatalogColumn>> {
        self.columns
            .get(&(owner.to_owned(), name.to_owned()))
            .cloned()
            .map_or(Lookup::Pending, Lookup::Ready)
    }

    fn members(&self, owner: &str, name: &str) -> Lookup<Vec<CatalogMember>> {
        self.members
            .get(&(owner.to_owned(), name.to_owned()))
            .cloned()
            .map_or(Lookup::Pending, Lookup::Ready)
    }
}

/// Wraps a catalog and records pending lookups as requests.
pub(crate) struct Tracker<'a> {
    inner: &'a dyn Catalog,
    missing: RefCell<Vec<CatalogRequest>>,
}

impl<'a> Tracker<'a> {
    pub fn new(inner: &'a dyn Catalog) -> Self {
        Self {
            inner,
            missing: RefCell::new(Vec::new()),
        }
    }

    fn pending<T>(&self, lookup: Lookup<T>, request: impl FnOnce() -> CatalogRequest) -> Option<T> {
        match lookup {
            Lookup::Ready(v) => Some(v),
            Lookup::Pending => {
                let request = request();
                let mut missing = self.missing.borrow_mut();
                if !missing.contains(&request) {
                    missing.push(request);
                }
                None
            }
        }
    }

    pub fn names(&self) -> NameStyle {
        self.inner.names()
    }

    pub fn schemas(&self) -> Option<Vec<String>> {
        self.pending(self.inner.schemas(), || CatalogRequest::Schemas)
    }

    pub fn objects(&self, schema: Option<&str>) -> Option<Vec<CatalogObject>> {
        self.pending(self.inner.objects(schema), || CatalogRequest::Objects {
            schema: schema.map(str::to_owned),
        })
    }

    /// `None` = pending; `Some(None)` = not an object.
    pub fn resolve(&self, schema: Option<&str>, name: &str) -> Option<Option<CatalogObject>> {
        self.pending(self.inner.resolve(schema, name), || {
            CatalogRequest::Resolve {
                schema: schema.map(str::to_owned),
                name: name.to_owned(),
            }
        })
    }

    pub fn columns(&self, owner: &str, name: &str) -> Option<Vec<CatalogColumn>> {
        self.pending(self.inner.columns(owner, name), || {
            CatalogRequest::Columns {
                owner: owner.to_owned(),
                name: name.to_owned(),
            }
        })
    }

    pub fn members(&self, owner: &str, name: &str) -> Option<Vec<CatalogMember>> {
        self.pending(self.inner.members(owner, name), || {
            CatalogRequest::Members {
                owner: owner.to_owned(),
                name: name.to_owned(),
            }
        })
    }

    pub fn into_missing(self) -> Vec<CatalogRequest> {
        self.missing.into_inner()
    }
}
