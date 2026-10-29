//! Dictionary lookups for code completion and navigation.
//!
//! `pythia_lsp_core` reports what it is missing as [`CatalogRequest`]s; each is
//! answered by a task on the connection's main session and stored in the
//! connection's [`CatalogCache`]. Names follow Oracle's storage rules:
//! the engine already normalizes unquoted identifiers to uppercase.

use pythia_db::oracle::Connection;
use pythia_db::{DbError, DbKind, DbRequest, Dialect, TaskFn};
use pythia_lsp_core::{
    CatalogCache, CatalogColumn, CatalogMember, CatalogObject, CatalogParam, CatalogRequest,
    MemberKind, ParamMode,
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
}

/// Builds the task answering `request` on Oracle.
pub fn task(request: CatalogRequest) -> TaskFn {
    Box::new(move |conn: &Connection| {
        let answer = fetch(conn, request)?;
        Ok(Box::new(answer) as Box<dyn std::any::Any + Send>)
    })
}

/// The request answering `request` (with an [`Answer`]) on a connection of
/// `dialect`.
pub fn request(id: u64, request: CatalogRequest, dialect: Dialect) -> DbRequest {
    match dialect.kind() {
        DbKind::Oracle => DbRequest::Task {
            id,
            task: task(request),
        },
        DbKind::MySql | DbKind::Postgres | DbKind::SqlServer => DbRequest::Run {
            id,
            task: Box::new(move |client| {
                let answer = dictionary::fetch(client, request)?;
                Ok(Box::new(answer) as Box<dyn std::any::Any + Send>)
            }),
        },
    }
}

/// The dictionaries of MySQL, PostgreSQL and SQL Server. Code assistance
/// asks with uppercase names (Oracle's rule for unquoted ones); these
/// databases keep their case (PostgreSQL folds unquoted names to
/// lowercase), so names are compared without it and answered as stored.
mod dictionary {
    use pythia_db::{Cell, Client, DbError, DbKind};
    use pythia_lsp_core::{CatalogColumn, CatalogObject, CatalogRequest};

    use super::Answer;

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
        objects: "select table_schema as owner, table_name as name, \
                         if(table_type = 'VIEW', 'VIEW', 'TABLE') as type \
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

    pub(super) fn fetch(
        client: &mut dyn Client,
        request: CatalogRequest,
    ) -> Result<Answer, DbError> {
        let sql = Sql::of(client.dialect().kind());
        Ok(match request {
            CatalogRequest::Schemas => {
                let rows = client.query(sql.schemas, &[])?;
                Answer::Schemas(rows.into_iter().filter_map(first).collect())
            }
            CatalogRequest::Objects { schema } => {
                let owner = owner(client, sql, schema.as_deref())?;
                let rows = client.query(
                    &format!(
                        "select * from ({}) o where lower(owner) = lower({})",
                        sql.objects,
                        sql.param(1)
                    ),
                    &[&owner],
                )?;
                Answer::Objects(schema, rows.into_iter().filter_map(object).collect())
            }
            CatalogRequest::Resolve { schema, name } => {
                let owner = owner(client, sql, schema.as_deref())?;
                let rows = client.query(
                    &format!(
                        "select * from ({}) o \
                          where lower(owner) = lower({}) and lower(name) = lower({}) \
                          order by case when name = {} then 0 else 1 end, \
                                   case when type = 'TABLE' then 0 else 1 end {}",
                        sql.objects,
                        sql.param(1),
                        sql.param(2),
                        sql.param(3),
                        sql.first_row
                    ),
                    &[&owner, &name, &name],
                )?;
                let object = rows.into_iter().find_map(object);
                Answer::Resolved(schema, name, object)
            }
            CatalogRequest::Columns { owner, name } => {
                let rows = client.query(sql.columns, &[&owner, &name])?;
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

    /// The schema asked for, or the current one (MySQL's database).
    fn owner(client: &mut dyn Client, sql: &Sql, schema: Option<&str>) -> Result<String, DbError> {
        if let Some(schema) = schema {
            return Ok(schema.to_owned());
        }
        let rows = client.query(sql.current, &[])?;
        Ok(rows.into_iter().find_map(first).unwrap_or_default())
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

fn fetch(conn: &Connection, request: CatalogRequest) -> Result<Answer, DbError> {
    Ok(match request {
        CatalogRequest::Schemas => {
            let rows =
                conn.query_as::<String>("select username from all_users order by username", &[])?;
            Answer::Schemas(rows.collect::<Result<_, _>>()?)
        }
        CatalogRequest::Objects { schema } => {
            let objects = objects(conn, schema.as_deref())?;
            Answer::Objects(schema, objects)
        }
        CatalogRequest::Resolve { schema, name } => {
            let object = resolve(conn, schema.as_deref(), &name)?;
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

fn current_schema(conn: &Connection) -> Result<String, DbError> {
    Ok(conn.query_row_as(
        "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
        &[],
    )?)
}

/// Public synonyms read at most: a database has some ten thousand, Java's
/// (with a `/`) left out.
const PUBLIC_SYNONYMS: usize = 100_000;

/// Objects of a schema, all of them; for the current schema (`None`) then
/// public synonyms, so `dbms_` completes to the supplied packages. One
/// capped query of both dropped an arbitrary part of a large schema (#161).
fn objects(conn: &Connection, schema: Option<&str>) -> Result<Vec<CatalogObject>, DbError> {
    let owner = match schema {
        Some(s) => s.to_owned(),
        None => current_schema(conn)?,
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
) -> Result<Option<CatalogObject>, DbError> {
    let owner = match schema {
        Some(s) => s.to_owned(),
        None => current_schema(conn)?,
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
                fetch(conn, request).expect("catalog query").apply(cache);
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
        let objects = objects(&conn, None).unwrap();
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
        let mut cache = CatalogCache::new(current_schema(&conn).unwrap());

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

    use pythia_db::{ConnectParams, DbEvent, Session};
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
            self.session.send(super::request(7, request, self.dialect));
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
            self.session.send(DbRequest::Execute {
                id: 1,
                sql: sql.into(),
                max_rows: 10,
                binds: Vec::new(),
            });
            loop {
                match self.next() {
                    DbEvent::Executed { .. } => return,
                    DbEvent::Failed { error, .. } => panic!("{sql}: {error}"),
                    _ => {}
                }
            }
        }
    }

    /// Completes the columns of `cat_orders` after its alias, created by
    /// `create` on the database of `env`; returns `(label, inserted text)`.
    fn complete_columns(
        env: &str,
        kind: DbKind,
        names: NameStyle,
        create: &str,
    ) -> Option<Vec<(String, String)>> {
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
        live.run("drop table if exists cat_orders");
        live.run(create);

        let mut cache = CatalogCache::new(server.current_schema).with_names(names);
        // Code assistance looks names up in uppercase.
        let src = "select o.\n  from CAT_ORDERS o";
        let analysis = Analysis::new(src);
        let mut items = Vec::new();
        for _ in 0..8 {
            let c = analysis.complete(9, &cache);
            if c.missing.is_empty() {
                items = c.items;
                break;
            }
            for request in c.missing {
                live.answer(request).apply(&mut cache);
            }
        }
        live.run("drop table cat_orders");
        Some(
            items
                .into_iter()
                .map(|i| (i.label, i.insert_text))
                .collect(),
        )
    }

    fn has(items: &[(String, String)], label: &str, text: &str) -> bool {
        items.iter().any(|(l, t)| l == label && t == text)
    }

    #[test]
    fn mysql_columns_after_an_alias() {
        let Some(items) = complete_columns(
            "PYTHIA_TEST_MYSQL",
            DbKind::MySql,
            NameStyle::Backticks,
            "create table cat_orders (id int primary key, Customer varchar(30), `order` int)",
        ) else {
            return;
        };
        // Names as stored; backticks only where MySQL needs them.
        assert!(has(&items, "Customer", "Customer"), "{items:?}");
        assert!(has(&items, "order", "`order`"), "{items:?}");
    }

    #[test]
    fn postgres_columns_after_an_alias() {
        let Some(items) = complete_columns(
            "PYTHIA_TEST_POSTGRES",
            DbKind::Postgres,
            NameStyle::Lower,
            "create table cat_orders (id int primary key, \"Customer\" varchar(30), \"order\" int)",
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
        let Some(items) = complete_columns(
            "PYTHIA_TEST_MSSQL",
            DbKind::SqlServer,
            NameStyle::Brackets,
            "create table cat_orders (id int primary key, Customer varchar(30), [order] int)",
        ) else {
            return;
        };
        // Names as stored; brackets only where SQL Server needs them.
        assert!(has(&items, "Customer", "Customer"), "{items:?}");
        assert!(has(&items, "order", "[order]"), "{items:?}");
    }
}
