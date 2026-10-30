//! Data dictionary queries: schemas, objects, columns, source, DDL, errors.
//!
//! Everything here reads `ALL_*` views so it works for any user, and only
//! returns what the user is allowed to see.

use crate::oracle::Connection;

use crate::types::{Cell, Column, DbError};

/// Object types shown in the object browser.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum ObjectType {
    Table,
    View,
    MaterializedView,
    Sequence,
    Synonym,
    Package,
    PackageBody,
    Procedure,
    Function,
    Type,
    TypeBody,
    Trigger,
    Index,
    DatabaseLink,
    JavaSource,
    Job,
    Library,
    Directory,
    /// Users, roles and profiles aren't schema objects: their `ObjectRef`
    /// has an empty owner.
    User,
    Role,
    Profile,
}

impl ObjectType {
    /// Types listed as folders in the object browser, in display order.
    pub const BROWSER: &[ObjectType] = &[
        ObjectType::Table,
        ObjectType::View,
        ObjectType::MaterializedView,
        ObjectType::Package,
        ObjectType::Procedure,
        ObjectType::Function,
        ObjectType::Type,
        ObjectType::Trigger,
        ObjectType::Sequence,
        ObjectType::Synonym,
        ObjectType::Index,
        ObjectType::DatabaseLink,
        ObjectType::JavaSource,
        ObjectType::Job,
        ObjectType::Library,
    ];

    /// What the pseudo-schema `PUBLIC` owns, as browser folders.
    pub const PUBLIC: &[ObjectType] = &[ObjectType::Synonym, ObjectType::DatabaseLink];

    /// Name as used in `ALL_OBJECTS.OBJECT_TYPE` and `DBMS_METADATA`.
    pub fn dictionary_name(self) -> &'static str {
        match self {
            ObjectType::Table => "TABLE",
            ObjectType::View => "VIEW",
            ObjectType::MaterializedView => "MATERIALIZED VIEW",
            ObjectType::Sequence => "SEQUENCE",
            ObjectType::Synonym => "SYNONYM",
            ObjectType::Package => "PACKAGE",
            ObjectType::PackageBody => "PACKAGE BODY",
            ObjectType::Procedure => "PROCEDURE",
            ObjectType::Function => "FUNCTION",
            ObjectType::Type => "TYPE",
            ObjectType::TypeBody => "TYPE BODY",
            ObjectType::Trigger => "TRIGGER",
            ObjectType::Index => "INDEX",
            ObjectType::DatabaseLink => "DATABASE LINK",
            ObjectType::JavaSource => "JAVA SOURCE",
            ObjectType::Job => "JOB",
            ObjectType::Library => "LIBRARY",
            ObjectType::Directory => "DIRECTORY",
            ObjectType::User => "USER",
            ObjectType::Role => "ROLE",
            ObjectType::Profile => "PROFILE",
        }
    }

    pub fn from_dictionary(name: &str) -> Option<Self> {
        ObjectType::ALL
            .iter()
            .copied()
            .find(|t| t.dictionary_name() == name)
    }

    const ALL: &[ObjectType] = &[
        ObjectType::Table,
        ObjectType::View,
        ObjectType::MaterializedView,
        ObjectType::Sequence,
        ObjectType::Synonym,
        ObjectType::Package,
        ObjectType::PackageBody,
        ObjectType::Procedure,
        ObjectType::Function,
        ObjectType::Type,
        ObjectType::TypeBody,
        ObjectType::Trigger,
        ObjectType::Index,
        ObjectType::DatabaseLink,
        ObjectType::JavaSource,
        ObjectType::Job,
        ObjectType::Library,
        ObjectType::Directory,
        ObjectType::User,
        ObjectType::Role,
        ObjectType::Profile,
    ];

    /// Folder label in the object browser.
    pub fn plural(self) -> &'static str {
        match self {
            ObjectType::Table => "Tables",
            ObjectType::View => "Views",
            ObjectType::MaterializedView => "Materialized views",
            ObjectType::Sequence => "Sequences",
            ObjectType::Synonym => "Synonyms",
            ObjectType::Package => "Packages",
            ObjectType::PackageBody => "Package bodies",
            ObjectType::Procedure => "Procedures",
            ObjectType::Function => "Functions",
            ObjectType::Type => "Types",
            ObjectType::TypeBody => "Type bodies",
            ObjectType::Trigger => "Triggers",
            ObjectType::Index => "Indexes",
            ObjectType::DatabaseLink => "Database links",
            ObjectType::JavaSource => "Java sources",
            ObjectType::Job => "Scheduler jobs",
            ObjectType::Library => "Libraries",
            ObjectType::Directory => "Directories",
            ObjectType::User => "Users",
            ObjectType::Role => "Roles",
            ObjectType::Profile => "Profiles",
        }
    }

    /// Objects that belong to the database rather than a schema (the
    /// browser lists them in sections of their own; their `ObjectRef`
    /// has an empty owner).
    pub fn is_database_object(self) -> bool {
        matches!(
            self,
            ObjectType::User | ObjectType::Role | ObjectType::Profile | ObjectType::Directory
        )
    }

    /// Stored PL/SQL (or Java) source that can be opened in a program window.
    pub fn has_source(self) -> bool {
        matches!(
            self,
            ObjectType::Package
                | ObjectType::PackageBody
                | ObjectType::Procedure
                | ObjectType::Function
                | ObjectType::Type
                | ObjectType::TypeBody
                | ObjectType::Trigger
                | ObjectType::JavaSource
        )
    }

    /// Objects whose data can be queried.
    pub fn has_data(self) -> bool {
        matches!(
            self,
            ObjectType::Table | ObjectType::View | ObjectType::MaterializedView
        )
    }

    /// The type of the body belonging to a spec, if any.
    pub fn body(self) -> Option<ObjectType> {
        match self {
            ObjectType::Package => Some(ObjectType::PackageBody),
            ObjectType::Type => Some(ObjectType::TypeBody),
            _ => None,
        }
    }

    /// Name used by `DBMS_METADATA.GET_DDL` (differs for a few types).
    fn metadata_name(self) -> &'static str {
        match self {
            ObjectType::Package => "PACKAGE_SPEC",
            ObjectType::PackageBody => "PACKAGE_BODY",
            ObjectType::Type => "TYPE_SPEC",
            ObjectType::TypeBody => "TYPE_BODY",
            ObjectType::MaterializedView => "MATERIALIZED_VIEW",
            ObjectType::DatabaseLink => "DB_LINK",
            ObjectType::JavaSource => "JAVA_SOURCE",
            ObjectType::Job => "PROCOBJ",
            other => other.dictionary_name(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbObject {
    pub owner: String,
    pub name: String,
    pub object_type: ObjectType,
    pub valid: bool,
    /// `LAST_DDL_TIME` as `YYYY-MM-DD HH24:MI:SS`.
    pub last_ddl: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumn {
    pub name: String,
    /// Full type, e.g. `VARCHAR2(30 CHAR)`, `NUMBER(10,2)`.
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
    /// Position in the primary key, if part of it.
    pub pk_position: Option<u32>,
}

/// A procedure or function inside a package or type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub overload: Option<String>,
    pub is_function: bool,
    /// Parameter list for display, e.g. `(p_id IN NUMBER, p_name IN VARCHAR2)`.
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompileError {
    pub object_type: String,
    pub name: String,
    pub line: u32,
    pub position: u32,
    pub text: String,
    pub warning: bool,
}

#[derive(Debug, Clone)]
pub enum MetaQuery {
    Schemas,
    Objects {
        owner: String,
        object_type: ObjectType,
        filter: Option<String>,
    },
    Columns {
        owner: String,
        table: String,
        /// Also the primary key positions: a dictionary join only worth it
        /// for tables.
        keys: bool,
    },
    Members {
        owner: String,
        package: String,
    },
    /// Indexes, constraints, triggers, grants... of a table as a generic table.
    Details {
        owner: String,
        name: String,
        detail: Detail,
    },
    Source {
        owner: String,
        name: String,
        object_type: ObjectType,
    },
    Ddl {
        owner: String,
        name: String,
        object_type: ObjectType,
    },
    Errors {
        owner: String,
        name: String,
        object_type: ObjectType,
    },
    /// Objects matching a name pattern across all visible schemas.
    Find {
        pattern: String,
        limit: usize,
    },
    /// `EXPLAIN PLAN` for a statement, returned as a table.
    ExplainPlan {
        sql: String,
    },
    /// Queries over a database link: the remote database's global name.
    /// Rolls back the session's transaction, so it runs on the metadata
    /// session.
    TestLink {
        owner: String,
        name: String,
    },
    /// The database's users, roles, profiles or directories (those of
    /// [`crate::Dialect::account_types`]).
    Accounts(ObjectType),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Detail {
    Indexes,
    Constraints,
    Triggers,
    Grants,
    Uses,
    UsedBy,
    /// A database link's user, host and creation time.
    Link,
}

impl Detail {
    pub const TABLE: &[Detail] = &[
        Detail::Indexes,
        Detail::Constraints,
        Detail::Triggers,
        Detail::Grants,
        Detail::UsedBy,
    ];
    pub const CODE: &[Detail] = &[Detail::Uses, Detail::UsedBy, Detail::Grants];
    pub const LINK: &[Detail] = &[Detail::Link];

    pub fn label(self) -> &'static str {
        match self {
            Detail::Indexes => "Indexes",
            Detail::Constraints => "Constraints",
            Detail::Triggers => "Triggers",
            Detail::Grants => "Grants",
            Detail::Uses => "Uses",
            Detail::UsedBy => "Used by",
            Detail::Link => "Details",
        }
    }
}

#[derive(Debug, Clone)]
pub enum MetaResult {
    Schemas(Vec<String>),
    Objects(Vec<DbObject>),
    Columns(Vec<TableColumn>),
    Members(Vec<Member>),
    Table {
        columns: Vec<Column>,
        rows: Vec<Vec<Cell>>,
    },
    Text(String),
    Errors(Vec<CompileError>),
    /// Names of users, roles, profiles or directories, each with what else
    /// to show (a directory's path).
    Accounts(Vec<(String, String)>),
}

pub(crate) fn run(conn: &Connection, query: &MetaQuery) -> Result<MetaResult, DbError> {
    Ok(match query {
        MetaQuery::Schemas => {
            let rows =
                conn.query_as::<String>("select username from all_users order by username", &[])?;
            MetaResult::Schemas(rows.collect::<Result<_, _>>()?)
        }
        MetaQuery::Objects {
            owner,
            object_type,
            filter,
        } => MetaResult::Objects(objects(conn, owner, *object_type, filter.as_deref())?),
        MetaQuery::Columns { owner, table, keys } => {
            MetaResult::Columns(columns(conn, owner, table, *keys)?)
        }
        MetaQuery::Members { owner, package } => {
            MetaResult::Members(members(conn, owner, package)?)
        }
        MetaQuery::Details {
            owner,
            name,
            detail,
        } => details(conn, owner, name, *detail)?,
        MetaQuery::Source {
            owner,
            name,
            object_type,
        } => MetaResult::Text(object_source(conn, owner, name, *object_type)?),
        MetaQuery::Ddl {
            owner,
            name,
            object_type,
        } => MetaResult::Text(object_ddl(conn, owner, name, *object_type, true)?),
        MetaQuery::Errors {
            owner,
            name,
            object_type,
        } => MetaResult::Errors(errors(conn, owner, name, object_type.dictionary_name())?),
        MetaQuery::Find { pattern, limit } => MetaResult::Objects(find(conn, pattern, *limit)?),
        MetaQuery::ExplainPlan { sql } => explain_plan(conn, sql)?,
        MetaQuery::TestLink { owner, name } => MetaResult::Text(test_link(conn, owner, name)?),
        MetaQuery::Accounts(object_type) => MetaResult::Accounts(accounts(conn, *object_type)?),
    })
}

/// Names of the database's users, of its roles (all of them with catalog
/// access, else those granted to the session's user), of its profiles
/// (with catalog access), or of the directories the session can use (with
/// their paths); each with what else to show.
fn accounts(conn: &Connection, object_type: ObjectType) -> Result<Vec<(String, String)>, DbError> {
    let read = |sql: &str| -> Result<Vec<(String, String)>, DbError> {
        let mut out = Vec::new();
        for row in conn.query_as::<(String, Option<String>)>(sql, &[])? {
            let (name, detail) = row?;
            out.push((name, detail.unwrap_or_default()));
        }
        Ok(out)
    };
    match object_type {
        ObjectType::User => read("select username, null from all_users order by username"),
        ObjectType::Directory => read(
            "select directory_name, directory_path from all_directories order by directory_name",
        ),
        ObjectType::Profile => {
            read("select distinct profile, null from dba_profiles order by profile")
                .map_err(|_| DbError::new("Listing profiles needs access to DBA_PROFILES"))
        }
        ObjectType::Role => read("select role, null from dba_roles order by role").or_else(|_| {
            read("select granted_role, null from user_role_privs order by granted_role")
        }),
        other => Err(DbError::new(format!("{} aren't accounts", other.plural()))),
    }
}

const OBJECT_COLUMNS: &str =
    "owner, object_name, object_type, status, to_char(last_ddl_time, 'YYYY-MM-DD HH24:MI:SS')";

fn object_rows(
    rows: crate::oracle::ResultSet<'static, crate::oracle::Row>,
) -> Result<Vec<DbObject>, DbError> {
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        let (owner, name, ty, status, last_ddl): (String, String, String, String, Option<String>) =
            row.get_as()?;
        let Some(object_type) = ObjectType::from_dictionary(&ty) else {
            continue;
        };
        out.push(DbObject {
            owner,
            name,
            object_type,
            valid: status == "VALID",
            last_ddl: last_ddl.unwrap_or_default(),
        });
    }
    Ok(out)
}

fn objects(
    conn: &Connection,
    owner: &str,
    object_type: ObjectType,
    filter: Option<&str>,
) -> Result<Vec<DbObject>, DbError> {
    // Scheduler jobs aren't in ALL_OBJECTS under a type of their own name
    // in every version; the dictionary name still matches on 12c+.
    let pattern = like_pattern(filter.unwrap_or(""));
    if object_type == ObjectType::DatabaseLink {
        return links(conn, owner, &pattern);
    }
    let sql = format!(
        "select {OBJECT_COLUMNS} from all_objects \
         where owner = :1 and object_type = :2 and object_name like :3 escape '\\' \
           and object_name not like 'BIN$%' and generated = 'N' \
         order by object_name"
    );
    object_rows(conn.query(&sql, &[&owner, &object_type.dictionary_name(), &pattern])?)
}

/// Database links from `ALL_OBJECTS`, which has other schemas' private
/// links the user may see on 23ai, plus the links it leaves out from
/// `DBA_DB_LINKS` where the user may read it (other schemas' private links
/// before 23ai, #51), else from `ALL_DB_LINKS`. Links have no
/// `LAST_DDL_TIME`: the creation time stands in.
fn links(conn: &Connection, owner: &str, pattern: &str) -> Result<Vec<DbObject>, DbError> {
    let sql = |view: &str| {
        format!(
            "select owner, object_name, object_type, status, \
                    to_char(nvl(last_ddl_time, created), 'YYYY-MM-DD HH24:MI:SS') \
               from all_objects \
              where owner = :owner and object_type = 'DATABASE LINK' \
                and object_name like :pattern escape '\\' \
             union all \
             select l.owner, l.db_link, 'DATABASE LINK', 'VALID', \
                    to_char(l.created, 'YYYY-MM-DD HH24:MI:SS') \
               from {view} l \
              where l.owner = :owner and l.db_link like :pattern escape '\\' \
                and not exists (select 1 from all_objects o \
                                 where o.owner = l.owner and o.object_name = l.db_link \
                                   and o.object_type = 'DATABASE LINK') \
              order by 2"
        )
    };
    let params: [(&str, &dyn crate::oracle::sql_type::ToSql); 2] =
        [("owner", &owner), ("pattern", &pattern)];
    let rows = match conn.query_named(&sql("dba_db_links"), &params) {
        // ORA-00942: no access to DBA_DB_LINKS.
        Err(e) if e.db_error().is_some_and(|d| d.code() == 942) => {
            conn.query_named(&sql("all_db_links"), &params)?
        }
        rows => rows?,
    };
    object_rows(rows)
}

fn find(conn: &Connection, pattern: &str, limit: usize) -> Result<Vec<DbObject>, DbError> {
    let types: Vec<String> = ObjectType::BROWSER
        .iter()
        .map(|t| format!("'{}'", t.dictionary_name()))
        .collect();
    let sql = format!(
        "select {OBJECT_COLUMNS} from all_objects \
         where object_name like :1 escape '\\' and object_type in ({}) \
         order by case when owner = sys_context('USERENV', 'CURRENT_SCHEMA') then 0 else 1 end, \
                  length(object_name), object_name, owner",
        types.join(", ")
    );
    let sql = crate::compat::first_rows(&sql, limit);
    object_rows(conn.query(&sql, &[&like_pattern(pattern)])?)
}

/// Turns a user filter into a LIKE pattern: case-insensitive "contains",
/// `*` and `?` as wildcards, `%`/`_` matched literally.
pub fn like_pattern(filter: &str) -> String {
    let mut out = String::from("%");
    for c in filter.trim().to_uppercase().chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '*' => out.push('%'),
            '?' => out.push('_'),
            c => out.push(c),
        }
    }
    out.push('%');
    out
}

fn columns(
    conn: &Connection,
    owner: &str,
    table: &str,
    keys: bool,
) -> Result<Vec<TableColumn>, DbError> {
    let pk = if keys {
        "(select p.position from all_cons_columns p join all_constraints k \
            on k.owner = p.owner and k.constraint_name = p.constraint_name \
           where k.constraint_type = 'P' and k.owner = c.owner and k.table_name = c.table_name \
             and p.column_name = c.column_name)"
    } else {
        "cast(null as number)"
    };
    let sql = format!(
        "select c.column_name, \
                case \
                  when c.data_type in ('VARCHAR2', 'NVARCHAR2', 'CHAR', 'NCHAR', 'RAW') then \
                    c.data_type || '(' || decode(c.char_used, 'C', c.char_length || ' CHAR', c.data_length) || ')' \
                  when c.data_type = 'NUMBER' and c.data_precision is not null then \
                    'NUMBER(' || c.data_precision || nvl2(nullif(c.data_scale, 0), ',' || c.data_scale, '') || ')' \
                  when c.data_type = 'NUMBER' and c.data_scale = 0 then 'INTEGER' \
                  else c.data_type end, \
                c.nullable, c.data_default, cc.comments, {pk} \
               from all_tab_columns c \
               left join all_col_comments cc \
                 on cc.owner = c.owner and cc.table_name = c.table_name and cc.column_name = c.column_name \
              where c.owner = :1 and c.table_name = :2 \
              order by c.column_id"
    );
    let mut out = Vec::new();
    for row in conn.query(&sql, &[&owner, &table])? {
        let row = row?;
        let (name, data_type, nullable, default, comment, pk): (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<u32>,
        ) = row.get_as()?;
        out.push(TableColumn {
            name,
            data_type,
            nullable: nullable == "Y",
            default: default
                .map(|d| d.trim().to_owned())
                .filter(|d| !d.is_empty()),
            comment,
            pk_position: pk,
        });
    }
    Ok(out)
}

fn members(conn: &Connection, owner: &str, package: &str) -> Result<Vec<Member>, DbError> {
    // 11g has a row without a name for a parameterless subprogram.
    let sql = format!(
        "select p.procedure_name, p.overload, \
                (select count(*) from all_arguments a \
                  where a.owner = p.owner and a.package_name = p.object_name \
                    and a.object_name = p.procedure_name \
                    and nvl(a.overload, '-') = nvl(p.overload, '-') \
                    and a.position = 0 and a.data_level = 0), \
                (select listagg(lower(a.argument_name) || ' ' || a.in_out || ' ' || \
                        {}, ', ') \
                        within group (order by a.position) \
                   from all_arguments a \
                  where a.owner = p.owner and a.package_name = p.object_name \
                    and a.object_name = p.procedure_name \
                    and nvl(a.overload, '-') = nvl(p.overload, '-') \
                    and a.position > 0 and a.data_level = 0 and a.argument_name is not null) \
               from all_procedures p \
              where p.owner = :1 and p.object_name = :2 and p.procedure_name is not null \
              order by p.subprogram_id",
        crate::compat::ARGUMENT_TYPE
    );
    let mut out = Vec::new();
    for row in conn.query(&sql, &[&owner, &package])? {
        let row = row?;
        let (name, overload, returns, args): (String, Option<String>, u32, Option<String>) =
            row.get_as()?;
        out.push(Member {
            name,
            overload,
            is_function: returns > 0,
            signature: args.map(|a| format!("({a})")).unwrap_or_default(),
        });
    }
    Ok(out)
}

fn details(
    conn: &Connection,
    owner: &str,
    name: &str,
    detail: Detail,
) -> Result<MetaResult, DbError> {
    let sql = match detail {
        Detail::Indexes => {
            "select i.index_name, i.index_type, i.uniqueness, \
                    listagg(c.column_name, ', ') within group (order by c.column_position) as columns, \
                    i.status, i.tablespace_name \
               from all_indexes i \
               join all_ind_columns c on c.index_owner = i.owner and c.index_name = i.index_name \
              where i.table_owner = :1 and i.table_name = :2 \
              group by i.index_name, i.index_type, i.uniqueness, i.status, i.tablespace_name \
              order by i.index_name"
        }
        Detail::Constraints => return constraints(conn, owner, name),
        Detail::Triggers => {
            "select trigger_name, trigger_type, triggering_event, status \
               from all_triggers where table_owner = :1 and table_name = :2 order by trigger_name"
        }
        Detail::Grants => {
            "select grantee, privilege, grantable, grantor \
               from all_tab_privs where table_schema = :1 and table_name = :2 \
              order by grantee, privilege"
        }
        Detail::Uses => {
            "select distinct referenced_owner as owner, referenced_name as name, referenced_type as type \
               from all_dependencies \
              where owner = :1 and name = :2 and referenced_owner not in ('SYS', 'PUBLIC') \
              order by 1, 3, 2"
        }
        Detail::UsedBy => {
            "select distinct owner, name, type from all_dependencies \
              where referenced_owner = :1 and referenced_name = :2 \
              order by 1, 3, 2"
        }
        Detail::Link => return link_details(conn, owner, name),
    };
    table_result(conn.query(sql, &[&owner, &name])?, &[])
}

/// Constraints of a table, without the NOT NULL checks of its columns.
/// The condition is `SEARCH_CONDITION_VC` from 12.1 on; before, there is
/// only the LONG `SEARCH_CONDITION`, which SQL can't filter, so the NOT
/// NULL checks are left out here.
fn constraints(conn: &Connection, owner: &str, name: &str) -> Result<MetaResult, DbError> {
    let condition = if crate::compat::at_least(conn, 12, 1)? {
        "k.search_condition_vc"
    } else {
        "k.search_condition"
    };
    let sql = format!(
        "select k.constraint_name, \
                decode(k.constraint_type, 'P', 'Primary key', 'U', 'Unique', 'R', 'Foreign key', \
                       'C', 'Check', k.constraint_type) as type, \
                (select listagg(c.column_name, ', ') within group (order by c.position) \
                   from all_cons_columns c \
                  where c.owner = k.owner and c.constraint_name = k.constraint_name) as columns, \
                (select r.table_name from all_constraints r \
                  where r.owner = k.r_owner and r.constraint_name = k.r_constraint_name) as references, \
                {condition} as condition, k.status, k.delete_rule \
           from all_constraints k \
          where k.owner = :1 and k.table_name = :2 \
          order by decode(k.constraint_type, 'P', 1, 'U', 2, 'R', 3, 4), k.constraint_name"
    );
    let MetaResult::Table { columns, mut rows } =
        table_result(conn.query(&sql, &[&owner, &name])?, &[])?
    else {
        unreachable!("table_result returns a table")
    };
    rows.retain(|r| {
        !(r[1].as_deref() == Some("Check")
            && r[4].as_deref().is_some_and(|c| c.ends_with(" IS NOT NULL")))
    });
    Ok(MetaResult::Table { columns, rows })
}

/// A database link as property/value rows. `ALL_DB_LINKS` has the user's
/// own and public links; another schema's private link comes from
/// `DBA_DB_LINKS` where the user may read it.
fn link_details(conn: &Connection, owner: &str, name: &str) -> Result<MetaResult, DbError> {
    type Link = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let read = |view: &str| -> Result<Option<Link>, crate::oracle::Error> {
        let sql = format!(
            "select owner, db_link, username, host, to_char(created, 'YYYY-MM-DD HH24:MI:SS') \
               from {view} where owner = :1 and db_link = :2"
        );
        conn.query_as::<Link>(&sql, &[&owner, &name])?
            .next()
            .transpose()
    };
    let link = match read("all_db_links")? {
        Some(link) => Some(link),
        // ORA-00942: no access to DBA_DB_LINKS.
        None => read("dba_db_links").or_else(|e| match e.db_error().map(|d| d.code()) {
            Some(942) => Ok(None),
            _ => Err(e),
        })?,
    };
    let column = |name: &str| Column {
        name: name.into(),
        type_name: "VARCHAR2".into(),
        numeric: false,
    };
    let rows = match link {
        Some((owner, name, user, host, created)) => vec![
            ("Owner", Some(owner)),
            ("Name", Some(name)),
            ("Connects as", user),
            ("Host", host),
            ("Created", created),
        ]
        .into_iter()
        .map(|(property, value)| vec![Some(property.to_owned()), value])
        .collect(),
        None => Vec::new(),
    };
    Ok(MetaResult::Table {
        columns: vec![column("PROPERTY"), column("VALUE")],
        rows,
    })
}

/// Runs a query over a database link and closes it again. Only the user's
/// own and public links can be used; a private link of the same name
/// takes precedence over a public one.
fn test_link(conn: &Connection, owner: &str, name: &str) -> Result<String, DbError> {
    let user = conn.query_row_as::<String>("select user from dual", &[])?;
    if owner != "PUBLIC" && owner != user {
        return Err(DbError::new(format!(
            "{owner}'s private database links can only be used by {owner}"
        )));
    }
    let link = format!("\"{}\"", name.replace('"', "\"\""));
    let result =
        conn.query_row_as::<String>(&format!("select global_name from global_name@{link}"), &[]);
    // A remote query opens a transaction; end it before closing the link.
    let _ = conn.rollback();
    let _ = conn.execute(&format!("alter session close database link {link}"), &[]);
    Ok(result?)
}

fn explain_plan(conn: &Connection, sql: &str) -> Result<MetaResult, DbError> {
    let statement_id = format!("PYTHIA{}", std::process::id());
    conn.execute(
        "delete from plan_table where statement_id = :1",
        &[&statement_id],
    )?;
    conn.execute(
        &format!("explain plan set statement_id = '{statement_id}' for {sql}"),
        &[],
    )?;
    let rows = conn.query(
        "select id, parent_id, depth, operation || nvl2(options, ' ' || options, '') as operation, \
                nvl2(object_name, nvl2(object_owner, object_owner || '.', '') || object_name, '') as object, \
                cost, cardinality as \"ROWS\", bytes, time, \
                access_predicates as access_pred, filter_predicates as filter_pred \
           from plan_table where statement_id = :1 order by id",
        &[&statement_id],
    )?;
    let result = table_result(
        rows,
        &["ID", "PARENT_ID", "DEPTH", "COST", "ROWS", "BYTES", "TIME"],
    );
    conn.execute(
        "delete from plan_table where statement_id = :1",
        &[&statement_id],
    )?;
    conn.rollback()?;
    result
}

/// The plan of a cursor in the shared pool with the row source statistics
/// of its last execution (`GATHER_PLAN_STATISTICS` or `STATISTICS_LEVEL =
/// ALL`), as a table with [`MetaQuery::ExplainPlan`]'s columns plus STARTS,
/// A_ROWS, A_TIME (seconds), BUFFERS and READS. Those are empty without
/// statistics, and 0 while the first execution is still running.
pub fn cursor_plan(conn: &Connection, sql_id: &str, child: i64) -> Result<MetaResult, DbError> {
    let rows = conn.query(
        "select id, parent_id, depth, operation || nvl2(options, ' ' || options, '') as operation, \
                nvl2(object_name, nvl2(object_owner, object_owner || '.', '') || object_name, '') as object, \
                cost, cardinality as \"ROWS\", bytes, time, \
                access_predicates as access_pred, filter_predicates as filter_pred, \
                last_starts as starts, last_output_rows as a_rows, \
                last_elapsed_time / 1000000 as a_time, \
                last_cr_buffer_gets + last_cu_buffer_gets as buffers, last_disk_reads as reads \
           from v$sql_plan_statistics_all \
          where sql_id = :1 and child_number = :2 order by id",
        &[&sql_id, &child],
    )?;
    table_result(
        rows,
        &[
            "ID",
            "PARENT_ID",
            "DEPTH",
            "COST",
            "ROWS",
            "BYTES",
            "TIME",
            "STARTS",
            "A_ROWS",
            "A_TIME",
            "BUFFERS",
            "READS",
        ],
    )
}

/// DBMS_XPLAN.DISPLAY_CURSOR's report of child `child` of `sql_id` (of
/// every child with `None`) in `format`, a line per row. DBMS_XPLAN prints
/// `V$SQL.SQL_TEXT`, the statement's first 1000 characters; the report has
/// the whole statement (`SQL_FULLTEXT`) instead (#178).
pub fn display_cursor(
    conn: &Connection,
    sql_id: &str,
    child: Option<i64>,
    format: &str,
) -> Result<Vec<String>, DbError> {
    let lines = conn.query_as::<Option<String>>(
        "select plan_table_output from table(dbms_xplan.display_cursor(:1, :2, :3))",
        &[&sql_id, &child, &format],
    )?;
    let lines = lines
        .map(|l| l.map(Option::unwrap_or_default))
        .collect::<Result<Vec<_>, _>>()?;
    // A cursor that aged out since keeps the report as DBMS_XPLAN made it.
    let text = conn
        .query_row_as::<Option<String>>(
            "select sql_fulltext from v$sql where sql_id = :1 and rownum = 1",
            &[&sql_id],
        )
        .ok()
        .flatten();
    Ok(match text {
        Some(text) => full_statement(lines, &text),
        None => lines,
    })
}

/// DBMS_XPLAN's `report` with `text` in place of each statement in it that
/// it cut short, the beginning of `text`.
fn full_statement(mut report: Vec<String>, text: &str) -> Vec<String> {
    // DBMS_XPLAN breaks the statement's lines anywhere, and turns its line
    // breaks into spaces.
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let whole = squash(text);
    let mut i = 0;
    while i + 1 < report.len() {
        // "SQL_ID  …, child number 0" ("EXPLAINED SQL STATEMENT:" in BASIC),
        // underlined, then the statement up to the plan hash value.
        let header =
            report[i].starts_with("SQL_ID") || report[i].starts_with("EXPLAINED SQL STATEMENT");
        let underline = &report[i + 1];
        if !header || underline.len() < 3 || underline.chars().any(|c| c != '-') {
            i += 1;
            continue;
        }
        let start = i + 2;
        let Some(end) = report[start..]
            .iter()
            .position(|l| l.starts_with("Plan hash value"))
            .map(|n| start + n)
        else {
            break;
        };
        let mut stop = end;
        while stop > start && report[stop - 1].trim().is_empty() {
            stop -= 1;
        }
        let shown = squash(&report[start..stop].concat());
        i = end;
        if whole.len() > shown.len() && whole.starts_with(&shown) {
            let lines = wrap_statement(text);
            i = start + lines.len();
            report.splice(start..stop, lines);
        }
    }
    report
}

/// `text` as DBMS_XPLAN shows a statement: line breaks as spaces, on lines
/// of up to 72 characters that end after a space where there is one.
fn wrap_statement(text: &str) -> Vec<String> {
    const WIDTH: usize = 72;
    let chars: Vec<char> = text
        .trim_end()
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let mut lines = Vec::new();
    let mut rest = &chars[..];
    while rest.len() > WIDTH {
        let cut = rest[..WIDTH]
            .iter()
            .rposition(|&c| c == ' ')
            .map_or(WIDTH, |p| p + 1);
        lines.push(rest[..cut].iter().collect());
        rest = &rest[cut..];
    }
    lines.push(rest.iter().collect());
    lines
}

/// Collects a result set into a generic table; `numeric` names right-aligned columns.
fn table_result(
    rows: crate::oracle::ResultSet<'static, crate::oracle::Row>,
    numeric: &[&str],
) -> Result<MetaResult, DbError> {
    let columns = rows
        .column_info()
        .iter()
        .map(|c| Column {
            name: c.name().to_owned(),
            type_name: c.oracle_type().to_string(),
            numeric: numeric.contains(&c.name()),
        })
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        out.push(
            row.sql_values()
                .iter()
                .map(|v| {
                    if v.is_null().unwrap_or(true) {
                        None
                    } else {
                        v.get::<String>().ok()
                    }
                })
                .collect(),
        );
    }
    Ok(MetaResult::Table { columns, rows: out })
}

/// Source of a stored unit as a runnable `CREATE OR REPLACE` statement
/// (without the terminating `/`).
pub fn object_source(
    conn: &Connection,
    owner: &str,
    name: &str,
    object_type: ObjectType,
) -> Result<String, DbError> {
    let rows = conn.query_as::<Option<String>>(
        "select text from all_source where owner = :1 and name = :2 and type = :3 order by line",
        &[&owner, &name, &object_type.dictionary_name()],
    )?;
    let mut text = String::new();
    for line in rows {
        text.push_str(&line?.unwrap_or_default());
    }
    if text.is_empty() {
        return Err(DbError::new(format!(
            "No source found for {} {owner}.{name}",
            object_type.dictionary_name().to_lowercase()
        )));
    }
    // ALL_SOURCE starts with e.g. `PACKAGE BODY foo AS`; make it runnable.
    Ok(format!("CREATE OR REPLACE {}", text.trim_end()))
}

/// DDL of an object from DBMS_METADATA (tables with comments and indexes),
/// without storage clauses; `schema` keeps the owner in the names.
pub fn object_ddl(
    conn: &Connection,
    owner: &str,
    name: &str,
    object_type: ObjectType,
    schema: bool,
) -> Result<String, DbError> {
    conn.execute(
        "begin \
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'EMIT_SCHEMA', :1 = 1); \
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'SQLTERMINATOR', true); \
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'PRETTY', true); \
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'SEGMENT_ATTRIBUTES', false); \
           dbms_metadata.set_transform_param(dbms_metadata.session_transform, 'STORAGE', false); \
         end;",
        &[&i32::from(schema)],
    )?;
    let ddl: String = conn.query_row_as(
        "select dbms_metadata.get_ddl(:1, :2, :3) from dual",
        &[&object_type.metadata_name(), &name, &owner],
    )?;
    let mut ddl = ddl.trim().to_owned();
    if object_type == ObjectType::Table {
        // Append comments and indexes the way PL/SQL Developer's "View" does.
        for (kind, sql) in [
            (
                "COMMENT",
                "select dbms_metadata.get_dependent_ddl('COMMENT', :1, :2) from dual",
            ),
            (
                "INDEX",
                "select dbms_metadata.get_dependent_ddl('INDEX', :1, :2) from dual",
            ),
        ] {
            // ORA-31608: no dependent objects of this kind.
            match conn.query_row_as::<String>(sql, &[&name, &owner]) {
                Ok(dep) => {
                    ddl.push_str("\n\n");
                    ddl.push_str(dep.trim());
                }
                Err(e) if e.db_error().is_some_and(|d| d.code() == 31608) => {}
                Err(e) => tracing::warn!("dependent {kind} ddl: {e}"),
            }
        }
    }
    Ok(ddl)
}

pub(crate) fn errors(
    conn: &Connection,
    owner: &str,
    name: &str,
    object_type: &str,
) -> Result<Vec<CompileError>, DbError> {
    let rows = conn.query(
        "select type, name, line, position, text, attribute from all_errors \
          where owner = :1 and name = :2 and (:3 is null or type = :3) \
          order by type, sequence",
        &[&owner, &name, &object_type],
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (object_type, name, line, position, text, attribute): (
            String,
            String,
            u32,
            u32,
            String,
            String,
        ) = row?.get_as()?;
        out.push(CompileError {
            object_type,
            name,
            line,
            position,
            text: text.trim_end().to_owned(),
            warning: attribute == "WARNING",
        });
    }
    if out.iter().any(|e| e.object_type == "TRIGGER") {
        let offset = trigger_body_line(conn, owner, name)? - 1;
        for e in out.iter_mut().filter(|e| e.object_type == "TRIGGER") {
            e.line += offset;
        }
    }
    Ok(out)
}

/// The line of a trigger's source (`ALL_SOURCE`) on which its PL/SQL block
/// starts, 1 if there is none. Oracle counts a trigger's error lines from
/// there (#114).
pub(crate) fn trigger_body_line(
    conn: &Connection,
    owner: &str,
    name: &str,
) -> Result<u32, DbError> {
    let rows = conn.query_as::<Option<String>>(
        "select text from all_source where owner = :1 and name = :2 and type = 'TRIGGER' \
          order by line",
        &[&owner, &name],
    )?;
    let mut source = String::new();
    for line in rows {
        source.push_str(&line?.unwrap_or_default());
    }
    Ok(pythia_plsql::trigger_body_line(&source).map_or(1, |l| l as u32))
}

/// One step of another database's execution plan, for [`plan_table`].
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct PlanStep {
    /// Index of the parent step in the plan.
    pub parent: Option<usize>,
    pub depth: usize,
    pub operation: String,
    pub object: Option<String>,
    pub cost: Option<String>,
    pub rows: Option<String>,
    pub access: Option<String>,
    pub filter: Option<String>,
}

/// A plan as the `PLAN_TABLE` rows the plan view reads (ids from 1).
pub(crate) fn plan_table(steps: Vec<PlanStep>) -> MetaResult {
    let column = |name: &str, numeric: bool| Column {
        name: name.into(),
        type_name: if numeric { "NUMBER" } else { "VARCHAR2" }.into(),
        numeric,
    };
    let columns = vec![
        column("ID", true),
        column("PARENT_ID", true),
        column("DEPTH", true),
        column("OPERATION", false),
        column("OBJECT", false),
        column("COST", true),
        column("ROWS", true),
        column("ACCESS_PRED", false),
        column("FILTER_PRED", false),
    ];
    let rows = steps
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            vec![
                Some((i + 1).to_string()),
                s.parent.map(|p| (p + 1).to_string()),
                Some(s.depth.to_string()),
                Some(s.operation),
                s.object,
                s.cost,
                s.rows,
                s.access,
                s.filter,
            ]
        })
        .collect();
    MetaResult::Table { columns, rows }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert_eq!(like_pattern(""), "%%");
        assert_eq!(like_pattern("emp"), "%EMP%");
        assert_eq!(like_pattern("emp*_hist"), "%EMP%\\_HIST%");
        assert_eq!(like_pattern("a?c"), "%A_C%");
    }

    #[test]
    fn dictionary_roundtrip() {
        for t in ObjectType::ALL {
            assert_eq!(ObjectType::from_dictionary(t.dictionary_name()), Some(*t));
        }
    }

    /// The test statement of #178: 1318 characters, over four lines.
    fn long_statement() -> String {
        let words = ('a'..='z')
            .map(|c| c.to_string().repeat(10))
            .collect::<Vec<_>>()
            .join(" ");
        let mut text = format!(
            "select /* PYTHIA_XPLAN_LONG */ count(*) from dual where '{words}' is not null"
        );
        for _ in 0..3 {
            text.push_str(&format!("\n  and '{words}' is not null"));
        }
        text + "\n  and 'the end of the statement' is not null"
    }

    /// What Oracle 23's DISPLAY_CURSOR printed for it: SQL_TEXT, its first
    /// 1000 characters.
    fn long_report(header: &[&str]) -> Vec<String> {
        let mut report: Vec<String> = header.iter().map(|l| l.to_string()).collect();
        report.extend(
            [
                "select /* PYTHIA_XPLAN_LONG */ count(*) from dual where 'aaaaaaaaaa ",
                "bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee ffffffffff gggggggggg ",
                "hhhhhhhhhh iiiiiiiiii jjjjjjjjjj kkkkkkkkkk llllllllll mmmmmmmmmm ",
                "nnnnnnnnnn oooooooooo pppppppppp qqqqqqqqqq rrrrrrrrrr ssssssssss ",
                "tttttttttt uuuuuuuuuu vvvvvvvvvv wwwwwwwwww xxxxxxxxxx yyyyyyyyyy ",
                "zzzzzzzzzz' is not null   and 'aaaaaaaaaa bbbbbbbbbb cccccccccc ",
                "dddddddddd eeeeeeeeee ffffffffff gggggggggg hhhhhhhhhh iiiiiiiiii ",
                "jjjjjjjjjj kkkkkkkkkk llllllllll mmmmmmmmmm nnnnnnnnnn oooooooooo ",
                "pppppppppp qqqqqqqqqq rrrrrrrrrr ssssssssss tttttttttt uuuuuuuuuu ",
                "vvvvvvvvvv wwwwwwwwww xxxxxxxxxx yyyyyyyyyy zzzzzzzzzz' is not null   ",
                "and 'aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee ffffffffff ",
                "gggggggggg hhhhhhhhhh iiiiiiiiii jjjjjjjjjj kkkkkkkkkk llllllllll ",
                "mmmmmmmmmm nnnnnnnnnn oooooooooo pppppppppp qqqqqqqqqq rrrrrrrrrr ",
                "ssssssssss tttttttttt uuuuuuuuuu vvvvvvvvvv wwwwwwwwww xxxxxxxxxx ",
                "yyyyyyyyyy zzzzzzzzzz' is not null   and 'aaaaaaaaaa bbbbbbbbbb ccc",
                " ",
                "Plan hash value: 3910148636",
                " ",
                "-----------------------------------------------------------------",
                "| Id  | Operation        | Name | Rows  | Cost (%CPU)| Time     |",
            ]
            .map(String::from),
        );
        report
    }

    /// The statement's lines in `report`, between the underline and the
    /// blank line before the plan hash value.
    fn statement_lines(report: &[String], from: usize) -> &[String] {
        let end = from
            + report[from..]
                .iter()
                .position(|l| l.trim().is_empty())
                .unwrap();
        &report[from..end]
    }

    #[test]
    fn full_statement_replaces_sql_text() {
        let text = long_statement();
        assert_eq!(text.len(), 1318);
        let header = [
            "SQL_ID  8v2gh9rhhvjxp, child number 0",
            "-------------------------------------",
        ];
        let report = long_report(&header);
        let full = full_statement(report.clone(), &text);

        assert_eq!(full[..2], report[..2]);
        let lines = statement_lines(&full, 2);
        assert!(lines.iter().all(|l| l.chars().count() <= 72), "{lines:#?}");
        assert!(
            lines
                .last()
                .unwrap()
                .ends_with("'the end of the statement' is not null"),
            "{lines:#?}"
        );
        // Nothing lost but the line breaks.
        assert_eq!(lines.concat(), text.replace('\n', " "));
        assert_eq!(full[full.len() - 5..], report[report.len() - 5..]);
    }

    #[test]
    fn full_statement_in_basic_and_every_child() {
        let text = long_statement();
        // BASIC has no SQL_ID line.
        let basic = long_report(&["EXPLAINED SQL STATEMENT:", "------------------------"]);
        let full = full_statement(basic, &text);
        assert!(
            statement_lines(&full, 2)
                .last()
                .unwrap()
                .ends_with("is not null")
        );

        // DISPLAY_CURSOR with no child: a report per child.
        let mut two = long_report(&["SQL_ID  8v2gh9rhhvjxp, child number 0", "-----"]);
        two.extend(long_report(&[
            "SQL_ID  8v2gh9rhhvjxp, child number 1",
            "-----",
        ]));
        let full = full_statement(two, &text);
        let second = full
            .iter()
            .position(|l| l.ends_with("child number 1"))
            .unwrap();
        for from in [2, second + 2] {
            assert_eq!(
                statement_lines(&full, from).concat(),
                text.replace('\n', " ")
            );
        }
    }

    #[test]
    fn full_statement_keeps_what_is_whole() {
        let report: Vec<String> = [
            "SQL_ID  4fz9cx8x0yb7m, child number 0",
            "-------------------------------------",
            "select * from dual where 1 = 1   and 2 = 2",
            " ",
            "Plan hash value: 272002086",
        ]
        .map(String::from)
        .to_vec();
        let text = "select * from dual where 1 = 1\n  and 2 = 2";
        assert_eq!(full_statement(report.clone(), text), report);
        // Another statement's report.
        assert_eq!(full_statement(report.clone(), &long_statement()), report);
        // A cursor DISPLAY_CURSOR doesn't find.
        let missing = vec![
            "SQL_ID: 4fz9cx8x0yb7m, child number: 0 cannot be found".to_owned(),
            " ".to_owned(),
        ];
        assert_eq!(full_statement(missing.clone(), text), missing);
    }

    #[test]
    fn wraps_like_dbms_xplan() {
        // A word longer than a line starts one and is cut, as Oracle did.
        let word = "x".repeat(80);
        assert_eq!(
            wrap_statement(&format!("select '{word}' from\tdual\n")),
            [
                "select ".to_owned(),
                format!("'{}", &word[..71]),
                format!("{}' from dual", &word[71..])
            ]
        );
        assert_eq!(wrap_statement("select 1 from dual"), ["select 1 from dual"]);
    }
}
