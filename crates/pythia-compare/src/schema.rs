//! Schema compare: collect object inventories and DDL from two schemas,
//! compare them and generate a sync script that turns the target into the
//! source.
//!
//! Collection runs per object type ([`collect_type`]) so a UI can show
//! progress and run both sides on their own sessions in parallel; the
//! comparison itself ([`compare`]) is pure.

use std::collections::BTreeMap;

use pythia_db::ObjectType;
use pythia_db::oracle::Connection;

use crate::Error;
use crate::ddl::{self, DdlStatement, Normalize};
use crate::script::{Phase, ScriptHeader, SyncStatement};
use crate::table::{self, TableDef, ident};

/// Object types schema compare understands, in display order.
pub const TYPES: &[ObjectType] = &[
    ObjectType::Table,
    ObjectType::View,
    ObjectType::MaterializedView,
    ObjectType::Sequence,
    ObjectType::Synonym,
    ObjectType::Type,
    ObjectType::TypeBody,
    ObjectType::Package,
    ObjectType::PackageBody,
    ObjectType::Procedure,
    ObjectType::Function,
    ObjectType::Trigger,
];

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub types: Vec<ObjectType>,
    /// Case-insensitive "contains" filter on object names (`*`/`?` wildcards).
    pub name_filter: String,
    pub normalize: Normalize,
    /// Emit `DROP` statements (and column drops) enabled instead of commented out.
    pub include_drops: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            types: TYPES.to_vec(),
            name_filter: String::new(),
            normalize: Normalize::default(),
            include_drops: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey {
    pub object_type: ObjectType,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSnapshot {
    /// Readable, normalized DDL (for display and diffing).
    pub ddl: String,
    /// The same DDL as executable statements.
    pub statements: Vec<DdlStatement>,
}

/// Everything collected for one object type of one schema.
#[derive(Debug, Clone, Default)]
pub struct TypeSnapshot {
    pub objects: Vec<(ObjectKey, ObjectSnapshot)>,
    pub tables: BTreeMap<String, TableDef>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SchemaSnapshot {
    pub schema: String,
    pub objects: BTreeMap<ObjectKey, ObjectSnapshot>,
    pub tables: BTreeMap<String, TableDef>,
    pub errors: Vec<String>,
}

impl SchemaSnapshot {
    pub fn new(schema: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            ..Self::default()
        }
    }

    pub fn add(&mut self, part: TypeSnapshot) {
        self.objects.extend(part.objects);
        self.tables.extend(part.tables);
        self.errors.extend(part.errors);
    }
}

/// Name used by `DBMS_METADATA.GET_DDL`.
fn metadata_name(t: ObjectType) -> &'static str {
    match t {
        ObjectType::Package => "PACKAGE_SPEC",
        ObjectType::PackageBody => "PACKAGE_BODY",
        ObjectType::Type => "TYPE_SPEC",
        ObjectType::TypeBody => "TYPE_BODY",
        ObjectType::MaterializedView => "MATERIALIZED_VIEW",
        other => other.dictionary_name(),
    }
}

/// Session-level DBMS_METADATA settings: no storage clutter, terminators,
/// no schema names, foreign keys handled structurally.
const TRANSFORMS: &[(&str, bool)] = &[
    ("SQLTERMINATOR", true),
    ("PRETTY", true),
    ("SEGMENT_ATTRIBUTES", false),
    ("STORAGE", false),
    ("TABLESPACE", false),
    ("EMIT_SCHEMA", false),
    ("REF_CONSTRAINTS", false),
    ("CONSTRAINTS_AS_ALTER", false),
];

fn set_transforms(conn: &Connection) -> Result<(), Error> {
    conn.execute(
        "begin dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'DEFAULT'); end;",
        &[],
    )?;
    for (name, value) in TRANSFORMS {
        // Older versions don't know every parameter; skip those.
        let sql = format!(
            "begin dbms_metadata.set_transform_param(dbms_metadata.session_transform, '{name}', {value}); \
             exception when others then null; end;"
        );
        conn.execute(&sql, &[])?;
    }
    Ok(())
}

/// Collects one object type of `schema`.
pub fn collect_type(
    conn: &Connection,
    schema: &str,
    object_type: ObjectType,
    options: &Options,
) -> Result<TypeSnapshot, Error> {
    set_transforms(conn)?;
    let pattern = pythia_db::like_pattern(&options.name_filter);
    let mut part = TypeSnapshot::default();
    let names = object_names(conn, schema, object_type, &pattern)?;
    if names.is_empty() {
        return Ok(part);
    }
    if object_type == ObjectType::Table {
        part.tables = table::load(conn, schema, None)?;
        part.tables.retain(|name, _| names.contains(name));
    }
    let ddl_of = |name: &str| -> Result<String, Error> {
        Ok(conn.query_row_as::<String>(
            "select dbms_metadata.get_ddl(:1, :2, :3) from dual",
            &[&metadata_name(object_type), &name, &schema],
        )?)
    };
    for name in names {
        let raw = match ddl_of(&name) {
            Ok(raw) => raw,
            Err(e) => {
                part.errors.push(format!(
                    "{} {name}: {e}",
                    object_type.dictionary_name().to_lowercase()
                ));
                continue;
            }
        };
        let mut text = ddl::display(&raw, schema);
        if let Some(def) = part.tables.get(&name) {
            text.push_str(&table_extras(def));
        }
        let statements = ddl::statements(&raw)
            .into_iter()
            .map(|s| DdlStatement {
                sql: ddl::display(&format!("{}\n/\n", s.sql), schema)
                    .trim_end()
                    .to_owned(),
                plsql: s.plsql,
            })
            .collect();
        part.objects.push((
            ObjectKey { object_type, name },
            ObjectSnapshot {
                ddl: text,
                statements,
            },
        ));
    }
    Ok(part)
}

fn object_names(
    conn: &Connection,
    schema: &str,
    object_type: ObjectType,
    pattern: &str,
) -> Result<Vec<String>, Error> {
    let extra = match object_type {
        // Materialized view containers show up as tables, too.
        ObjectType::Table => {
            " and not exists (select 1 from all_mviews m where m.owner = o.owner and m.mview_name = o.object_name) \
              and exists (select 1 from all_tables t where t.owner = o.owner and t.table_name = o.object_name \
                            and t.nested = 'NO' and t.secondary = 'N' and nvl(t.iot_type, '-') <> 'IOT_OVERFLOW')"
        }
        _ => "",
    };
    let sql = format!(
        "select object_name from all_objects o \
          where owner = :1 and object_type = :2 and upper(object_name) like :3 escape '\\' \
            and object_name not like 'BIN$%' and generated = 'N'{extra} \
          order by object_name"
    );
    let rows =
        conn.query_as::<String>(&sql, &[&schema, &object_type.dictionary_name(), &pattern])?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Foreign keys, indexes and comments of a table as statements, appended to
/// its DDL for display (the CREATE TABLE text has none of them).
fn table_extras(def: &TableDef) -> String {
    let empty = def.as_created();
    let diff = table::diff(def, &empty, false);
    let mut out = String::new();
    let mut statements = diff.statements;
    statements.sort_by_key(|s| s.phase);
    for s in statements {
        out.push('\n');
        out.push_str(&s.sql);
        out.push('\n');
    }
    out
}

/// Collects all requested types (convenience for tests and simple callers).
pub fn collect(
    conn: &Connection,
    schema: &str,
    options: &Options,
) -> Result<SchemaSnapshot, Error> {
    let mut snapshot = SchemaSnapshot::new(schema);
    for t in &options.types {
        snapshot.add(collect_type(conn, schema, *t, options)?);
    }
    Ok(snapshot)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Different,
    OnlyInSource,
    OnlyInTarget,
    Equal,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Different => "Different",
            Status::OnlyInSource => "Only in source",
            Status::OnlyInTarget => "Only in target",
            Status::Equal => "Equal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ObjectResult {
    pub key: ObjectKey,
    pub status: Status,
    pub source_ddl: Option<String>,
    pub target_ddl: Option<String>,
    /// Statements that synchronize this object.
    pub statements: Vec<SyncStatement>,
    pub notes: Vec<String>,
    /// Included in the generated script.
    pub included: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SchemaDiff {
    pub source_schema: String,
    pub target_schema: String,
    pub objects: Vec<ObjectResult>,
    pub errors: Vec<String>,
}

impl SchemaDiff {
    pub fn count(&self, status: Status) -> usize {
        self.objects.iter().filter(|o| o.status == status).count()
    }

    /// True if nothing needs synchronizing.
    pub fn is_equal(&self) -> bool {
        self.objects.iter().all(|o| o.status == Status::Equal)
    }

    /// The sync script for all included objects.
    pub fn script(&self, title: &str) -> String {
        let statements: Vec<SyncStatement> = self
            .objects
            .iter()
            .filter(|o| o.included)
            .flat_map(|o| o.statements.iter().cloned())
            .collect();
        let header = ScriptHeader {
            title: title.to_owned(),
            schema: Some(self.target_schema.clone()),
            recompile: !statements.is_empty(),
        };
        crate::script::render(&header, &statements)
    }
}

/// Compares two snapshots. Statements turn the target into the source.
pub fn compare(source: &SchemaSnapshot, target: &SchemaSnapshot, options: &Options) -> SchemaDiff {
    let mut diff = SchemaDiff {
        source_schema: source.schema.clone(),
        target_schema: target.schema.clone(),
        objects: Vec::new(),
        errors: source
            .errors
            .iter()
            .map(|e| format!("source: {e}"))
            .chain(target.errors.iter().map(|e| format!("target: {e}")))
            .collect(),
    };
    let mut keys: Vec<&ObjectKey> = source.objects.keys().chain(target.objects.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let s = source.objects.get(key);
        let t = target.objects.get(key);
        let result = match (s, t) {
            (Some(s), None) => ObjectResult {
                key: key.clone(),
                status: Status::OnlyInSource,
                source_ddl: Some(s.ddl.clone()),
                target_ddl: None,
                statements: create_statements(key, s, source),
                notes: Vec::new(),
                included: true,
            },
            (None, Some(t)) => ObjectResult {
                key: key.clone(),
                status: Status::OnlyInTarget,
                source_ddl: None,
                target_ddl: Some(t.ddl.clone()),
                statements: vec![drop_statement(key, options.include_drops)],
                notes: Vec::new(),
                included: true,
            },
            (Some(s), Some(t)) => {
                let (statements, notes) = if key.object_type == ObjectType::Table {
                    match (source.tables.get(&key.name), target.tables.get(&key.name)) {
                        (Some(sd), Some(td)) => {
                            let d = table::diff(sd, td, options.include_drops);
                            (d.statements, d.notes)
                        }
                        _ => (Vec::new(), vec!["table structure unavailable".into()]),
                    }
                } else if ddl::canonical(&s.ddl, options.normalize)
                    == ddl::canonical(&t.ddl, options.normalize)
                {
                    (Vec::new(), Vec::new())
                } else {
                    (replace_statements(key, s, t), Vec::new())
                };
                ObjectResult {
                    key: key.clone(),
                    status: if statements.is_empty() {
                        Status::Equal
                    } else {
                        Status::Different
                    },
                    source_ddl: Some(s.ddl.clone()),
                    target_ddl: Some(t.ddl.clone()),
                    statements,
                    notes,
                    included: true,
                }
            }
            (None, None) => continue,
        };
        diff.objects.push(result);
    }
    diff.objects
        .sort_by(|a, b| (a.status, &a.key).cmp(&(b.status, &b.key)));
    diff
}

fn phase_of(t: ObjectType) -> Phase {
    match t {
        ObjectType::Table => Phase::Tables,
        ObjectType::View | ObjectType::MaterializedView => Phase::Views,
        ObjectType::Sequence => Phase::Sequences,
        ObjectType::Synonym => Phase::Synonyms,
        ObjectType::Type => Phase::Types,
        ObjectType::TypeBody => Phase::TypeBodies,
        ObjectType::Package => Phase::PackageSpecs,
        ObjectType::PackageBody => Phase::PackageBodies,
        ObjectType::Trigger => Phase::Triggers,
        _ => Phase::Procedures,
    }
}

fn from_ddl(key: &ObjectKey, snapshot: &ObjectSnapshot) -> Vec<SyncStatement> {
    snapshot
        .statements
        .iter()
        .map(|s| SyncStatement {
            phase: phase_of(key.object_type),
            sql: s.sql.clone(),
            plsql: s.plsql,
            warning: None,
            enabled: true,
            object: key.name.clone(),
        })
        .collect()
}

/// Statements creating an object that only exists in the source.
fn create_statements(
    key: &ObjectKey,
    snapshot: &ObjectSnapshot,
    source: &SchemaSnapshot,
) -> Vec<SyncStatement> {
    let mut out = from_ddl(key, snapshot);
    for s in &mut out {
        s.sql = strip_system_constraint_names(&s.sql);
    }
    match key.object_type {
        ObjectType::Table => {
            // CREATE TABLE has columns, PK/UK/checks; foreign keys, indexes
            // and comments come from the structural diff against that.
            if let Some(def) = source.tables.get(&key.name) {
                let created = def.as_created();
                out.extend(table::diff(def, &created, false).statements);
            }
        }
        ObjectType::Sequence => {
            if let Some(first) = out.first_mut() {
                first.warning = Some("START WITH is the source's current value".into());
            }
        }
        _ => {}
    }
    out
}

/// Statements replacing an object whose DDL differs.
fn replace_statements(
    key: &ObjectKey,
    snapshot: &ObjectSnapshot,
    target: &ObjectSnapshot,
) -> Vec<SyncStatement> {
    let name = ident(&key.name);
    let phase = phase_of(key.object_type);
    match key.object_type {
        ObjectType::Sequence => {
            // Sequences can't be replaced; alter the differing attributes and
            // keep the current value.
            let first = |o: &ObjectSnapshot| {
                o.statements
                    .first()
                    .map(|s| s.sql.clone())
                    .unwrap_or_default()
            };
            let clauses = sequence_changes(&first(snapshot), &first(target));
            if clauses.is_empty() {
                return Vec::new();
            }
            let mut s = SyncStatement::new(
                phase,
                &key.name,
                format!("alter sequence {name} {}", clauses.join(" ")),
            );
            s.warning = Some("the current value is not changed".into());
            vec![s]
        }
        ObjectType::MaterializedView => {
            let mut drop =
                SyncStatement::new(phase, &key.name, format!("drop materialized view {name}"));
            drop.warning = Some("materialized view is recreated and refreshed".into());
            let mut out = vec![drop];
            out.extend(from_ddl(key, snapshot));
            out
        }
        ObjectType::Type => {
            let mut out = from_ddl(key, snapshot);
            if let Some(first) = out.first_mut() {
                first.warning = Some("fails with ORA-02303 if the type has dependents".into());
            }
            out
        }
        _ => from_ddl(key, snapshot),
    }
}

/// Parses `CREATE SEQUENCE` attributes into (attribute group, clause) pairs,
/// e.g. `("INCREMENT", "INCREMENT BY 2")`, `("CACHE", "NOCACHE")`.
fn sequence_clauses(create: &str) -> Vec<(&'static str, String)> {
    let canonical = ddl::canonical(create, Normalize::default());
    let words: Vec<&str> = canonical.split(' ').skip(3).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i].to_ascii_uppercase();
        let value = |n: usize| words.get(i + n).copied().unwrap_or_default();
        let (group, clause, len): (&'static str, String, usize) = match w.as_str() {
            "MINVALUE" => ("MIN", format!("MINVALUE {}", value(1)), 2),
            "NOMINVALUE" => ("MIN", w.clone(), 1),
            "MAXVALUE" => ("MAX", format!("MAXVALUE {}", value(1)), 2),
            "NOMAXVALUE" => ("MAX", w.clone(), 1),
            "INCREMENT" => ("INCREMENT", format!("INCREMENT BY {}", value(2)), 3),
            "CACHE" => ("CACHE", format!("CACHE {}", value(1)), 2),
            "NOCACHE" => ("CACHE", w.clone(), 1),
            "ORDER" | "NOORDER" => ("ORDER", w.clone(), 1),
            "CYCLE" | "NOCYCLE" => ("CYCLE", w.clone(), 1),
            "KEEP" | "NOKEEP" => ("KEEP", w.clone(), 1),
            "SCALE" | "NOSCALE" => ("SCALE", w.clone(), 1),
            "GLOBAL" | "SESSION" => ("SCOPE", w.clone(), 1),
            _ => {
                i += 1;
                continue;
            }
        };
        out.push((group, clause));
        i += len;
    }
    out
}

/// Clauses of `source` whose value differs from `target`.
fn sequence_changes(source: &str, target: &str) -> Vec<String> {
    let target = sequence_clauses(target);
    sequence_clauses(source)
        .into_iter()
        .filter(|(group, clause)| !target.iter().any(|(g, c)| g == group && c == clause))
        .map(|(_, clause)| clause)
        .collect()
}

fn drop_statement(key: &ObjectKey, enabled: bool) -> SyncStatement {
    let name = ident(&key.name);
    let sql = match key.object_type {
        ObjectType::Table => format!("drop table {name} cascade constraints purge"),
        ObjectType::Type => format!("drop type {name} force"),
        t => format!("drop {} {name}", t.dictionary_name().to_lowercase()),
    };
    let mut s = SyncStatement::new(Phase::DropObjects, &key.name, sql);
    s.enabled = enabled;
    s.warning = Some(match key.object_type {
        ObjectType::Table => "drops the table and all its data".into(),
        _ => "object only exists in the target".into(),
    });
    s
}

/// Removes `CONSTRAINT "SYS_C..."` so the target generates its own names.
fn strip_system_constraint_names(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(i) = rest.find("CONSTRAINT \"SYS_C") {
        out.push_str(&rest[..i]);
        let after = &rest[i + "CONSTRAINT \"".len()..];
        let end = after.find('"').map_or(after.len(), |e| e + 1);
        rest = after[end..].trim_start();
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(schema: &str, objects: &[(ObjectType, &str, &str)]) -> SchemaSnapshot {
        let mut s = SchemaSnapshot::new(schema);
        for (t, name, raw) in objects {
            let statements = ddl::statements(raw);
            s.objects.insert(
                ObjectKey {
                    object_type: *t,
                    name: (*name).into(),
                },
                ObjectSnapshot {
                    ddl: ddl::display(raw, schema),
                    statements,
                },
            );
        }
        s
    }

    #[test]
    fn statuses_and_script_order() {
        let source = snapshot(
            "A",
            &[
                (
                    ObjectType::Package,
                    "PKG",
                    "CREATE OR REPLACE PACKAGE \"PKG\" AS procedure p; END;\n/\n",
                ),
                (
                    ObjectType::PackageBody,
                    "PKG",
                    "CREATE OR REPLACE PACKAGE BODY \"PKG\" AS procedure p is begin null; end; END;\n/\n",
                ),
                (
                    ObjectType::View,
                    "V",
                    "CREATE OR REPLACE FORCE VIEW \"V\" (\"X\") AS select 1 x from dual;",
                ),
                (
                    ObjectType::Sequence,
                    "S",
                    "CREATE SEQUENCE \"S\" MINVALUE 1 INCREMENT BY 2 START WITH 41 CACHE 20 NOCYCLE;",
                ),
            ],
        );
        let target = snapshot(
            "B",
            &[
                (
                    ObjectType::Package,
                    "PKG",
                    "CREATE OR REPLACE PACKAGE \"PKG\" AS procedure p; END;\n/\n",
                ),
                (
                    ObjectType::View,
                    "V",
                    "CREATE OR REPLACE FORCE VIEW \"V\" (\"X\") AS select 2 x from dual;",
                ),
                (
                    ObjectType::Sequence,
                    "S",
                    "CREATE SEQUENCE \"S\" MINVALUE 1 INCREMENT BY 1 START WITH 7 CACHE 20 NOCYCLE;",
                ),
                (
                    ObjectType::Procedure,
                    "OLD",
                    "CREATE OR REPLACE PROCEDURE \"OLD\" is begin null; end;\n/\n",
                ),
            ],
        );
        let diff = compare(&source, &target, &Options::default());
        let statuses: Vec<(Status, &str)> = diff
            .objects
            .iter()
            .map(|o| (o.status, o.key.name.as_str()))
            .collect();
        assert_eq!(
            statuses,
            [
                (Status::Different, "V"),
                (Status::Different, "S"),
                (Status::OnlyInSource, "PKG"),
                (Status::OnlyInTarget, "OLD"),
                (Status::Equal, "PKG"),
            ]
        );
        let script = diff.script("test");
        let pos = |needle: &str| {
            script
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} missing in\n{script}"))
        };
        assert!(pos("alter session set current_schema = b;") < pos("alter sequence s"));
        assert!(pos("alter sequence s INCREMENT BY 2;") < pos("FORCE VIEW"));
        assert!(pos("FORCE VIEW") < pos("PACKAGE BODY \"PKG\""));
        assert!(
            script.contains("-- drop procedure old;"),
            "drops are disabled by default:\n{script}"
        );
        assert!(
            script.contains("PACKAGE BODY \"PKG\" AS procedure p is begin null; end; END;\n/\n")
        );

        let with_drops = compare(
            &source,
            &target,
            &Options {
                include_drops: true,
                ..Options::default()
            },
        );
        assert!(with_drops.script("t").contains("\ndrop procedure old;"));
    }

    #[test]
    fn equal_snapshots() {
        let a = snapshot(
            "A",
            &[(
                ObjectType::Procedure,
                "P",
                "CREATE OR REPLACE PROCEDURE \"A\".\"P\" is begin null; end;\n/\n",
            )],
        );
        let b = snapshot(
            "B",
            &[(
                ObjectType::Procedure,
                "P",
                "CREATE OR REPLACE PROCEDURE \"B\".\"P\" is\nbegin null;   end;\n/\n",
            )],
        );
        let diff = compare(&a, &b, &Options::default());
        assert!(diff.is_equal(), "{:?}", diff.objects);
        assert!(diff.script("x").contains("Nothing to synchronize"));
    }

    #[test]
    fn system_constraint_names() {
        assert_eq!(
            strip_system_constraint_names(
                "\"ID\" NUMBER CONSTRAINT \"SYS_C0081\" NOT NULL ENABLE, CONSTRAINT \"PK\" PRIMARY KEY"
            ),
            "\"ID\" NUMBER NOT NULL ENABLE, CONSTRAINT \"PK\" PRIMARY KEY"
        );
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
            "create or replace view \"R62_Cmp\" as select 1 as x from dual",
            &[],
        )
        .unwrap();
        let options = Options {
            name_filter: "r62_cm".into(),
            ..Options::default()
        };
        let part = collect_type(&conn, &user.to_uppercase(), ObjectType::View, &options).unwrap();
        let names: Vec<&str> = part.objects.iter().map(|(k, _)| k.name.as_str()).collect();
        assert_eq!(names, ["R62_Cmp"], "{:?}", part.errors);
        conn.execute("drop view \"R62_Cmp\"", &[]).unwrap();
    }
}
