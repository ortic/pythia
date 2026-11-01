//! SQL Server's answers to [`MetaQuery`]s, from the catalog views of the
//! connection's database. Schemas are the database's schemas; names are
//! stored as written and usually compared without case. Tables have no DDL
//! of their own, so it is put together from the catalog; routines, views
//! and triggers keep their source as written.

use std::collections::HashMap;

use crate::dialect::Dialect;
use crate::meta::{
    CompileError, DbObject, Detail, MetaQuery, MetaResult, ObjectType, PlanStep, TableColumn,
    like_pattern, plan_table,
};
use crate::mssql_session::Ms;
use crate::types::{Cell, DbError};

/// `sys.objects o`'s type as Pythia names it.
const TYPE: &str = "case when o.type = 'U' then 'TABLE' when o.type = 'V' then 'VIEW' \
                         when o.type in ('P', 'PC') then 'PROCEDURE' \
                         when o.type = 'TR' then 'TRIGGER' when o.type = 'SO' then 'SEQUENCE' \
                         when o.type = 'SN' then 'SYNONYM' \
                         when o.type in ('FN', 'IF', 'TF', 'FS', 'FT') then 'FUNCTION' end";

/// The object types the browser lists.
const TYPES: &str = "'U', 'V', 'P', 'PC', 'TR', 'SO', 'SN', 'FN', 'IF', 'TF', 'FS', 'FT'";

/// The column's type as written in DDL (`c` is `sys.columns`, `t`
/// `sys.types`).
const COLUMN_TYPE: &str = "case when t.name in ('varchar', 'char', 'varbinary', 'binary') \
                                  then t.name + '(' + case when c.max_length = -1 then 'max' \
                                       else cast(c.max_length as varchar(10)) end + ')' \
                                when t.name in ('nvarchar', 'nchar') \
                                  then t.name + '(' + case when c.max_length = -1 then 'max' \
                                       else cast(c.max_length / 2 as varchar(10)) end + ')' \
                                when t.name in ('decimal', 'numeric') \
                                  then t.name + '(' + cast(c.precision as varchar(10)) + ',' \
                                       + cast(c.scale as varchar(10)) + ')' \
                                when t.name in ('datetime2', 'time', 'datetimeoffset') \
                                  then t.name + '(' + cast(c.scale as varchar(10)) + ')' \
                                else t.name end";

pub(crate) fn run(ms: &mut Ms<'_>, query: &MetaQuery) -> Result<MetaResult, DbError> {
    Ok(match query {
        MetaQuery::Schemas => MetaResult::Schemas(
            ms.rows(
                "select name from sys.schemas where schema_id < 16384 or schema_id > 16399 \
                  order by name",
                &[],
            )?
            .into_iter()
            .filter_map(first)
            .collect(),
        ),
        MetaQuery::Objects {
            owner,
            object_type,
            filter,
        } => MetaResult::Objects(objects(ms, owner, *object_type, filter.as_deref())?),
        MetaQuery::Columns { owner, table, keys } => {
            MetaResult::Columns(columns(ms, owner, table, *keys)?)
        }
        // Routines live on their own, not in packages.
        MetaQuery::Members { .. } => MetaResult::Members(Vec::new()),
        MetaQuery::Details {
            owner,
            name,
            detail,
        } => details(ms, owner, name, *detail)?,
        MetaQuery::Source {
            owner,
            name,
            object_type,
        } => MetaResult::Text(ddl(ms, owner, name, *object_type, false)?),
        MetaQuery::Ddl {
            owner,
            name,
            object_type,
        } => MetaResult::Text(ddl(ms, owner, name, *object_type, true)?),
        // SQL Server reports errors when a routine is created.
        MetaQuery::Errors { .. } => MetaResult::Errors(Vec::<CompileError>::new()),
        MetaQuery::Find { pattern, limit } => MetaResult::Objects(find(ms, pattern, *limit)?),
        MetaQuery::ExplainPlan { sql } => explain_plan(ms, sql)?,
        MetaQuery::TestLink { .. } => {
            return Err(DbError::unsupported("Linked servers", Dialect::SQLSERVER));
        }
        MetaQuery::Accounts(object_type) => MetaResult::Accounts(accounts(ms, *object_type)?),
    })
}

/// The database's users (SQL, Windows and external ones, without the
/// schemas `sys` and `INFORMATION_SCHEMA` posing as users) or its roles.
/// Server logins are the server's, not the database's.
fn accounts(ms: &mut Ms<'_>, object_type: ObjectType) -> Result<Vec<(String, String)>, DbError> {
    let which = if object_type == ObjectType::User {
        "type in ('S', 'U', 'G', 'E', 'X') and name not in ('sys', 'INFORMATION_SCHEMA')"
    } else {
        "type = 'R'"
    };
    let sql = format!("select name from sys.database_principals where {which} order by name");
    Ok(ms
        .rows(&sql, &[])?
        .into_iter()
        .filter_map(first)
        .map(|name| (name, String::new()))
        .collect())
}

fn first(row: Vec<Cell>) -> Option<String> {
    row.into_iter().next().flatten()
}

/// `[schema].[name]`, for `object_id()`.
fn bracketed(owner: &str, name: &str) -> String {
    format!(
        "[{}].[{}]",
        owner.replace(']', "]]"),
        name.replace(']', "]]")
    )
}

/// Rows of `(owner, name, type, last change)` as objects.
fn object_rows(rows: Vec<Vec<Cell>>) -> Vec<DbObject> {
    rows.into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter().map(Option::unwrap_or_default);
            let (owner, name, ty) = (values.next()?, values.next()?, values.next()?);
            Some(DbObject {
                owner,
                name,
                object_type: ObjectType::from_dictionary(&ty)?,
                valid: true,
                last_ddl: values.next().unwrap_or_default(),
            })
        })
        .collect()
}

fn objects(
    ms: &mut Ms<'_>,
    owner: &str,
    object_type: ObjectType,
    filter: Option<&str>,
) -> Result<Vec<DbObject>, DbError> {
    let pattern = like_pattern(filter.unwrap_or(""));
    let rows = ms.rows(
        &format!(
            "select s.name, o.name, {TYPE}, convert(varchar(19), o.modify_date, 120) \
               from sys.objects o join sys.schemas s on s.schema_id = o.schema_id \
              where s.name = @P1 and upper(o.name) like @P2 escape '\\' \
                and o.type in ({TYPES}) and {TYPE} = @P3 and o.is_ms_shipped = 0 \
              order by o.name"
        ),
        &[owner, &pattern, object_type.dictionary_name()],
    )?;
    Ok(object_rows(rows))
}

/// Objects of every schema whose name matches; the default schema's first,
/// then the shortest names.
fn find(ms: &mut Ms<'_>, pattern: &str, limit: usize) -> Result<Vec<DbObject>, DbError> {
    let pattern = like_pattern(pattern);
    let rows = ms.rows(
        &format!(
            "select top ({limit}) s.name, o.name, {TYPE}, convert(varchar(19), o.modify_date, 120) \
               from sys.objects o join sys.schemas s on s.schema_id = o.schema_id \
              where upper(o.name) like @P1 escape '\\' and o.type in ({TYPES}) \
                and o.is_ms_shipped = 0 \
              order by case when s.name = schema_name() then 0 else 1 end, len(o.name), o.name, \
                       s.name"
        ),
        &[&pattern],
    )?;
    Ok(object_rows(rows))
}

fn columns(
    ms: &mut Ms<'_>,
    owner: &str,
    table: &str,
    keys: bool,
) -> Result<Vec<TableColumn>, DbError> {
    let pk = if keys {
        "(select ic.key_ordinal from sys.indexes i \
           join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
          where i.object_id = c.object_id and i.is_primary_key = 1 and ic.column_id = c.column_id)"
    } else {
        "null"
    };
    let rows = ms.rows(
        &format!(
            "select c.name, {COLUMN_TYPE}, c.is_nullable, object_definition(c.default_object_id), \
                    cast(p.value as nvarchar(4000)), {pk} \
               from sys.columns c join sys.types t on t.user_type_id = c.user_type_id \
               left join sys.extended_properties p on p.major_id = c.object_id \
                and p.minor_id = c.column_id and p.class = 1 and p.name = 'MS_Description' \
              where c.object_id = object_id(@P1) \
              order by c.column_id"
        ),
        &[&bracketed(owner, table)],
    )?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let mut values = row.into_iter();
            let mut next = || values.next().flatten();
            TableColumn {
                name: next().unwrap_or_default(),
                data_type: next().unwrap_or_default(),
                nullable: next().as_deref() == Some("1"),
                default: next(),
                comment: next(),
                pk_position: next().and_then(|p| p.parse().ok()),
            }
        })
        .collect())
}

fn details(
    ms: &mut Ms<'_>,
    owner: &str,
    name: &str,
    detail: Detail,
) -> Result<MetaResult, DbError> {
    let sql = match detail {
        Detail::Indexes => {
            "select i.name as INDEX_NAME, i.type_desc as INDEX_TYPE, \
                    case when i.is_primary_key = 1 then 'PRIMARY' when i.is_unique = 1 then 'UNIQUE' \
                         else 'NONUNIQUE' end as UNIQUENESS, \
                    cast(null as nvarchar(4000)) as COLUMNS, \
                    cast(null as nvarchar(4000)) as [INCLUDE], \
                    i.filter_definition as [WHERE] \
               from sys.indexes i where i.object_id = object_id(@P1) and i.type > 0 \
              order by i.is_primary_key desc, i.name"
        }
        Detail::Constraints => {
            "select name as CONSTRAINT_NAME, type as TYPE, definition as DEFINITION, \
                    ref as [REFERENCES] \
               from (select k.name, case k.type when 'PK' then 'Primary key' else 'Unique' end \
                            as type, cast(null as nvarchar(4000)) as definition, \
                            null as ref, case k.type when 'PK' then 1 else 2 end as ord \
                       from sys.key_constraints k where k.parent_object_id = object_id(@P1) \
                     union all \
                     select f.name, 'Foreign key', null, \
                            object_schema_name(f.referenced_object_id) + '.' \
                              + object_name(f.referenced_object_id), 3 \
                       from sys.foreign_keys f where f.parent_object_id = object_id(@P1) \
                     union all \
                     select c.name, 'Check', c.definition, null, 4 \
                       from sys.check_constraints c where c.parent_object_id = object_id(@P1) \
                     union all \
                     select d.name, 'Default', col_name(d.parent_object_id, d.parent_column_id) \
                              + ' ' + d.definition, null, 5 \
                       from sys.default_constraints d where d.parent_object_id = object_id(@P1)) x \
              order by ord, name"
        }
        Detail::Triggers => {
            "select t.name as TRIGGER_NAME, cast(null as nvarchar(4000)) as TRIGGERING_EVENT, \
                    case when t.is_instead_of_trigger = 1 then 'INSTEAD OF' else 'AFTER' end \
                      as TIMING, \
                    case when t.is_disabled = 1 then 'DISABLED' else 'ENABLED' end as STATUS \
               from sys.triggers t where t.parent_id = object_id(@P1) \
              order by t.name"
        }
        Detail::Grants => {
            "select user_name(p.grantee_principal_id) as GRANTEE, \
                    p.permission_name as PRIVILEGE, p.state_desc as STATE \
               from sys.database_permissions p \
              where p.class = 1 and p.major_id = object_id(@P1) \
              order by 1, 2"
        }
        Detail::Uses => {
            "select distinct coalesce(object_schema_name(d.referenced_id), d.referenced_schema_name, \
                                      schema_name()) as OWNER, \
                    d.referenced_entity_name as NAME, \
                    coalesce(case when o.type = 'U' then 'TABLE' when o.type = 'V' then 'VIEW' \
                                  when o.type in ('P', 'PC') then 'PROCEDURE' \
                                  when o.type = 'SO' then 'SEQUENCE' when o.type = 'SN' then 'SYNONYM' \
                                  when o.type in ('FN', 'IF', 'TF', 'FS', 'FT') then 'FUNCTION' end, \
                             '') as TYPE \
               from sys.sql_expression_dependencies d \
               left join sys.objects o on o.object_id = d.referenced_id \
              where d.referencing_id = object_id(@P1) \
              order by 1, 2"
        }
        Detail::UsedBy => {
            "select distinct object_schema_name(d.referencing_id) as OWNER, \
                    object_name(d.referencing_id) as NAME, \
                    case when o.type = 'V' then 'VIEW' when o.type in ('P', 'PC') then 'PROCEDURE' \
                         when o.type = 'TR' then 'TRIGGER' \
                         when o.type in ('FN', 'IF', 'TF', 'FS', 'FT') then 'FUNCTION' \
                         else o.type_desc end as TYPE \
               from sys.sql_expression_dependencies d \
               join sys.objects o on o.object_id = d.referencing_id \
              where d.referenced_id = object_id(@P1) \
             union \
             select object_schema_name(f.parent_object_id), object_name(f.parent_object_id), 'TABLE' \
               from sys.foreign_keys f where f.referenced_object_id = object_id(@P1) \
                and f.parent_object_id <> f.referenced_object_id \
              order by 1, 3, 2"
        }
        Detail::Link => return Err(DbError::unsupported("Linked servers", Dialect::SQLSERVER)),
    };
    let object = bracketed(owner, name);
    let (columns, mut rows) = ms.query(sql, &[&object])?;
    // The lists the queries leave NULL, by the row's name: an index's
    // columns and INCLUDE columns, a key's columns, a trigger's events.
    // Every column of a columnstore index (types 5 and 6) counts as
    // included: they are its columns, not INCLUDE columns (#252).
    let (lists, filled): (&str, &[(usize, &str)]) = match detail {
        Detail::Indexes => (
            "select i.name, case when ic.is_included_column = 0 or i.type in (5, 6) \
                                 then 'columns' else 'include' end, \
                    col_name(ic.object_id, ic.column_id) \
                      + case when ic.is_descending_key = 1 then ' DESC' else '' end \
               from sys.indexes i \
               join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
              where i.object_id = object_id(@P1) and i.type > 0 \
              order by ic.key_ordinal, ic.index_column_id",
            &[(3, "columns"), (4, "include")],
        ),
        Detail::Constraints => (
            "select k.name, 'columns', col_name(ic.object_id, ic.column_id), ic.key_ordinal \
               from sys.key_constraints k \
               join sys.index_columns ic on ic.object_id = k.parent_object_id \
                and ic.index_id = k.unique_index_id \
              where k.parent_object_id = object_id(@P1) \
             union all \
             select f.name, 'columns', col_name(fc.parent_object_id, fc.parent_column_id), \
                    fc.constraint_column_id \
               from sys.foreign_keys f \
               join sys.foreign_key_columns fc on fc.constraint_object_id = f.object_id \
              where f.parent_object_id = object_id(@P1) \
              order by 4",
            &[(2, "columns")],
        ),
        Detail::Triggers => (
            "select t.name, 'events', e.type_desc \
               from sys.triggers t join sys.trigger_events e on e.object_id = t.object_id \
              where t.parent_id = object_id(@P1) \
              order by e.type",
            &[(1, "events")],
        ),
        _ => ("", &[]),
    };
    if !filled.is_empty() {
        let lists = Lists::read(ms, lists, &object)?;
        for row in &mut rows {
            let Some(name) = row.first().cloned().flatten() else {
                continue;
            };
            for &(column, list) in filled {
                if let Some(cell @ None) = row.get_mut(column) {
                    *cell = lists.get(&name, list).map(str::to_owned);
                }
            }
        }
    }
    Ok(MetaResult::Table { columns, rows })
}

/// Lists of names, as `string_agg(…, ', ') within group (order by …)` made
/// them: SQL Server has string_agg from 2017 only, and below compatibility
/// level 110, as in a database from 2008, WITHIN GROUP is a syntax error
/// (#294). So the lists are joined here, from rows of (name, list, item)
/// in their order, by the name of the index, constraint or trigger and the
/// list's; NULL items are left out, as string_agg does. FOR XML PATH fails
/// on a name with a control character.
struct Lists(HashMap<(String, String), String>);

impl Lists {
    fn read(ms: &mut Ms<'_>, sql: &str, object: &str) -> Result<Self, DbError> {
        let mut lists: HashMap<(String, String), String> = HashMap::new();
        for row in ms.rows(sql, &[object])? {
            let mut values = row.into_iter();
            let (Some(Some(name)), Some(Some(list)), Some(Some(item))) =
                (values.next(), values.next(), values.next())
            else {
                continue;
            };
            let joined = lists.entry((name, list)).or_default();
            if !joined.is_empty() {
                joined.push_str(", ");
            }
            joined.push_str(&item);
        }
        Ok(Self(lists))
    }

    /// `name`'s `list`, joined; `None` without items.
    fn get(&self, name: &str, list: &str) -> Option<&str> {
        self.0
            .get(&(name.to_owned(), list.to_owned()))
            .map(String::as_str)
    }

    /// `before` and the list in parentheses; nothing without items.
    fn clause(&self, before: &str, name: &str, list: &str) -> String {
        self.get(name, list)
            .map(|items| format!("{before}({items})"))
            .unwrap_or_default()
    }
}

/// An object's DDL; `runnable` ends it with `GO`, as T-SQL scripts do.
fn ddl(
    ms: &mut Ms<'_>,
    owner: &str,
    name: &str,
    object_type: ObjectType,
    runnable: bool,
) -> Result<String, DbError> {
    let object = bracketed(owner, name);
    let missing = || {
        DbError::new(format!(
            "No {} {owner}.{name}",
            object_type.dictionary_name().to_lowercase()
        ))
    };
    let text = match object_type {
        ObjectType::Table => table_ddl(ms, &object)?,
        ObjectType::View | ObjectType::Procedure | ObjectType::Function | ObjectType::Trigger => ms
            .rows("select object_definition(object_id(@P1))", &[&object])?
            .into_iter()
            .find_map(first)
            .ok_or_else(|| {
                DbError::new(format!(
                    "No source for {owner}.{name}: it is encrypted, or it takes the VIEW \
                     DEFINITION permission to see it"
                ))
            })?
            .trim()
            .to_owned(),
        ObjectType::Sequence => {
            let row = ms
                .rows(
                    "select type_name(s.user_type_id), cast(s.start_value as varchar(40)), \
                            cast(s.increment as varchar(40)), cast(s.minimum_value as varchar(40)), \
                            cast(s.maximum_value as varchar(40)), s.is_cycling, s.is_cached, \
                            s.cache_size \
                       from sys.sequences s where s.object_id = object_id(@P1)",
                    &[&object],
                )?
                .into_iter()
                .next()
                .ok_or_else(missing)?;
            let v: Vec<String> = row.into_iter().map(Option::unwrap_or_default).collect();
            let cache = match (v[6].as_str(), v[7].as_str()) {
                ("0", _) => " no cache".to_owned(),
                (_, "") => String::new(),
                (_, size) => format!(" cache {size}"),
            };
            format!(
                "create sequence {object} as {}\n  start with {} increment by {}\n  \
                 minvalue {} maxvalue {}{}{cache};",
                v[0],
                v[1],
                v[2],
                v[3],
                v[4],
                if v[5] == "1" { " cycle" } else { "" }
            )
        }
        ObjectType::Synonym => {
            let base = ms
                .rows(
                    "select base_object_name from sys.synonyms where object_id = object_id(@P1)",
                    &[&object],
                )?
                .into_iter()
                .find_map(first)
                .ok_or_else(missing)?;
            format!("create synonym {object} for {base};")
        }
        other => {
            return Err(DbError::new(format!(
                "SQL Server has no {}",
                other.plural().to_lowercase()
            )));
        }
    };
    Ok(if runnable {
        format!("{text}\nGO\n")
    } else {
        text
    })
}

/// `CREATE TABLE` with its columns and constraints, then its other indexes;
/// a memory-optimized table declares its indexes in it.
fn table_ddl(ms: &mut Ms<'_>, object: &str) -> Result<String, DbError> {
    let columns = ms.rows(
        &format!(
            "select quotename(c.name) + ' ' \
                    + case when c.is_computed = 1 \
                        then 'as ' + cc.definition \
                             + case when cc.is_persisted = 1 then ' persisted' else '' end \
                        else {COLUMN_TYPE} \
                             + case when ic.object_id is not null \
                                 then ' identity(' + cast(ic.seed_value as varchar(40)) + ', ' \
                                      + cast(ic.increment_value as varchar(40)) + ')' else '' end \
                             + case when c.is_nullable = 1 then ' null' else ' not null' end \
                             + coalesce(' constraint ' + quotename(dc.name) + ' default ' \
                                        + dc.definition, '') end \
               from sys.columns c join sys.types t on t.user_type_id = c.user_type_id \
               left join sys.computed_columns cc on cc.object_id = c.object_id \
                and cc.column_id = c.column_id \
               left join sys.identity_columns ic on ic.object_id = c.object_id \
                and ic.column_id = c.column_id \
               left join sys.default_constraints dc on dc.parent_object_id = c.object_id \
                and dc.parent_column_id = c.column_id \
              where c.object_id = object_id(@P1) \
              order by c.column_id"
        ),
        &[object],
    )?;
    if columns.is_empty() {
        return Err(DbError::new(format!("No table {object}")));
    }
    // A memory-optimized table's durability (#302). CREATE INDEX can't make
    // its indexes: they are declared in CREATE TABLE.
    let durability = ms
        .rows(
            "select lower(durability_desc collate database_default) from sys.tables \
              where object_id = object_id(@P1) and is_memory_optimized = 1",
            &[object],
        )?
        .into_iter()
        .find_map(first);
    let options = index_options(ms, object)?;
    // A key's columns, a foreign key's and the ones it references, and the
    // columns of the indexes, quoted, as lists (see Lists).
    let lists = Lists::read(
        ms,
        "select k.name, 'columns', quotename(col_name(ic.object_id, ic.column_id)) \
                  + case when ic.is_descending_key = 1 then ' desc' else '' end, ic.key_ordinal \
           from sys.key_constraints k \
           join sys.index_columns ic on ic.object_id = k.parent_object_id \
            and ic.index_id = k.unique_index_id and ic.is_included_column = 0 \
          where k.parent_object_id = object_id(@P1) \
         union all \
         select f.name, 'columns', quotename(col_name(fc.parent_object_id, fc.parent_column_id)), \
                fc.constraint_column_id \
           from sys.foreign_keys f \
           join sys.foreign_key_columns fc on fc.constraint_object_id = f.object_id \
          where f.parent_object_id = object_id(@P1) \
         union all \
         select f.name, 'references', \
                quotename(col_name(fc.referenced_object_id, fc.referenced_column_id)), \
                fc.constraint_column_id \
           from sys.foreign_keys f \
           join sys.foreign_key_columns fc on fc.constraint_object_id = f.object_id \
          where f.parent_object_id = object_id(@P1) \
          order by 4",
        object,
    )?;
    // Each constraint as (name, its line up to the columns, what follows
    // them: a foreign key's REFERENCES and its actions, a key's index).
    let constraints = ms.rows(
        "select name, line, refs, actions, ix from ( \
           select k.name, 'constraint ' + quotename(k.name) \
                  + case k.type when 'PK' then ' primary key ' else ' unique ' end \
                  + lower(i.type_desc collate database_default) as line, \
                  null as refs, '' as actions, i.name as ix, \
                  case k.type when 'PK' then 1 else 2 end as ord \
             from sys.key_constraints k \
             join sys.indexes i on i.object_id = k.parent_object_id and i.index_id = k.unique_index_id \
            where k.parent_object_id = object_id(@P1) \
           union all \
           select f.name, 'constraint ' + quotename(f.name) + ' foreign key', \
                  ' references ' + quotename(object_schema_name(f.referenced_object_id)) + '.' \
                  + quotename(object_name(f.referenced_object_id)), \
                  case f.delete_referential_action when 1 then ' on delete cascade' \
                       when 2 then ' on delete set null' when 3 then ' on delete set default' \
                       else '' end \
                  + case f.update_referential_action when 1 then ' on update cascade' \
                         when 2 then ' on update set null' when 3 then ' on update set default' \
                         else '' end, null, 3 \
             from sys.foreign_keys f where f.parent_object_id = object_id(@P1) \
           union all \
           select c.name, 'constraint ' + quotename(c.name) + ' check ' + c.definition, null, '', \
                  null, 4 \
             from sys.check_constraints c where c.parent_object_id = object_id(@P1) \
         ) x order by ord, name",
        &[object],
    )?;
    let constraints = constraints.into_iter().filter_map(|row| {
        let mut values = row.into_iter();
        let mut next = || values.next().flatten();
        let (name, line, refs) = (next()?, next()?, next());
        let (actions, index) = (next().unwrap_or_default(), next());
        let columns = lists.clause(" ", &name, "columns");
        let references = refs
            .map(|refs| format!("{refs}{}", lists.clause(" ", &name, "references")))
            .unwrap_or_default();
        let with = index
            .and_then(|index| options.get(&index))
            .map_or("", String::as_str);
        Some(format!("{line}{columns}{references}{actions}{with}"))
    });
    let mut lines: Vec<String> = columns
        .into_iter()
        .filter_map(first)
        .chain(constraints)
        .collect();
    let mut indexes = Vec::new();
    // Rowstore (types 1 and 2) and columnstore indexes (5 and 6, #252). A
    // nonclustered columnstore index names its columns, which count as
    // included; a clustered one holds all of them and names none. XML (3)
    // and spatial indexes (4) follow, a secondary XML index after the
    // primary one it uses, and a columnstore index's ORDER, which SQL
    // Server has from 2022 (#265). A selective XML index has its
    // namespaces and paths, a secondary one the path it is on (#311).
    let ordered = ms
        .rows(
            "select col_length('sys.index_columns', 'column_store_order_ordinal')",
            &[],
        )?
        .into_iter()
        .find_map(first)
        .is_some();
    let order = if ordered {
        "union all \
         select i.name, 'order', quotename(col_name(ic.object_id, ic.column_id)), \
                ic.column_store_order_ordinal, 0 \
           from sys.indexes i \
           join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
          where i.object_id = object_id(@P1) and ic.column_store_order_ordinal > 0"
    } else {
        ""
    };
    // An index's columns, its INCLUDE columns and its ORDER, quoted; a
    // selective XML index's namespaces and paths, as they are declared.
    let lists = Lists::read(
        ms,
        &format!(
            "select i.name, case when i.type = 5 then null \
                                 when ic.is_included_column = 0 or i.type = 6 then 'columns' \
                                 when i.type = 2 then 'include' end, \
                    quotename(col_name(ic.object_id, ic.column_id)) \
                      + case when ic.is_descending_key = 1 then ' desc' else '' end, \
                    ic.key_ordinal, ic.index_column_id \
               from sys.indexes i \
               join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
              where i.object_id = object_id(@P1) \
             {order} \
             union all \
             select i.name, 'namespaces', \
                    case when n.is_default_uri = 1 then 'default ' else '' end \
                      + '''' + replace(n.uri, '''', '''''') + '''' \
                      + coalesce(' as ' + quotename(n.prefix), ''), \
                    row_number() over (order by n.is_default_uri, n.prefix), 0 \
               from sys.indexes i \
               join sys.selective_xml_index_namespaces n on n.object_id = i.object_id \
                and n.index_id = i.index_id \
              where i.object_id = object_id(@P1) \
             union all \
             select i.name, 'paths', \
                    quotename(c.name) + ' = ''' \
                      + replace(c.path collate database_default, '''', '''''') + '''' \
                      + case when c.path_type = 1 then ' as sql ' + {COLUMN_TYPE} \
                             when c.is_node = 1 then ' as xquery ''node()''' \
                             when c.is_xquery_type_inferred = 0 \
                                  and c.xquery_type_description is not null \
                               or c.is_xquery_max_length_inferred = 0 and c.xquery_max_length > 0 \
                               then ' as xquery' \
                                    + coalesce(' ''' + case when c.is_xquery_type_inferred = 0 \
                                                            then c.xquery_type_description \
                                                                 collate database_default end \
                                               + '''', '') \
                                    + case when c.is_xquery_max_length_inferred = 0 \
                                                and c.xquery_max_length > 0 \
                                           then ' maxlength(' \
                                                + cast(c.xquery_max_length as varchar(10)) + ')' \
                                           else '' end \
                             else '' end \
                      + case when c.is_singleton = 1 then ' singleton' else '' end, \
                    c.path_id, 0 \
               from sys.indexes i \
               join sys.selective_xml_index_paths c on c.object_id = i.object_id \
                and c.index_id = i.index_id \
               left join sys.types t on t.user_type_id = c.user_type_id \
              where i.object_id = object_id(@P1) \
              order by 4, 5"
        ),
        object,
    )?;
    // Each index as (name, quoted, UNIQUE, its kind, a secondary XML
    // index's USING, its WHERE) and a spatial index's tessellation. Hash
    // indexes (7) are a memory-optimized table's (#302).
    let rows = ms.rows(
        "select i.name, quotename(i.name), case when i.is_unique = 1 then 'unique ' else '' end, \
                case x.xml_index_type when 0 then 'primary xml' when 2 then 'selective xml' \
                     else lower(i.type_desc collate database_default) end, \
                coalesce(' using xml index ' + quotename(u.name) + ' for ' \
                         + coalesce(lower(x.secondary_type_desc collate database_default), \
                                    '(' + quotename(p.name) + ')'), ''), \
                coalesce(' where ' + i.filter_definition, ''), \
                lower(s.tessellation_scheme collate database_default), \
                t.bounding_box_xmin, t.bounding_box_ymin, t.bounding_box_xmax, t.bounding_box_ymax, \
                lower(t.level_1_grid_desc collate database_default), \
                lower(t.level_2_grid_desc collate database_default), \
                lower(t.level_3_grid_desc collate database_default), \
                lower(t.level_4_grid_desc collate database_default), t.cells_per_object \
           from sys.indexes i \
           left join sys.xml_indexes x on x.object_id = i.object_id and x.index_id = i.index_id \
           left join sys.indexes u on u.object_id = i.object_id \
            and u.index_id = x.using_xml_index_id \
           left join sys.selective_xml_index_paths p on p.object_id = i.object_id \
            and p.index_id = x.using_xml_index_id and p.path_id = x.path_id \
           left join sys.spatial_indexes s on s.object_id = i.object_id \
            and s.index_id = i.index_id \
           left join sys.spatial_index_tessellations t on t.object_id = i.object_id \
            and t.index_id = i.index_id \
          where i.object_id = object_id(@P1) and i.type in (1, 2, 3, 4, 5, 6, 7) \
            and i.is_primary_key = 0 and i.is_unique_constraint = 0 \
          order by case when x.using_xml_index_id is null then 0 else 1 end, i.name",
        &[object],
    )?;
    for row in rows {
        let mut values = row.into_iter();
        let mut next = || values.next().flatten();
        let (Some(name), Some(quoted)) = (next(), next()) else {
            continue;
        };
        let kind = format!(
            "{}{}",
            next().unwrap_or_default(),
            next().unwrap_or_default()
        );
        let (using, filter) = (next().unwrap_or_default(), next().unwrap_or_default());
        let spatial: Vec<Cell> = values.collect();
        let columns = lists.clause(" ", &name, "columns");
        let with = options.get(&name).map_or("", String::as_str);
        if durability.is_some() {
            lines.push(format!("index {quoted} {kind}{columns}{with}"));
            continue;
        }
        indexes.push(format!(
            "create {kind} index {quoted} on {object}{columns}{}{}{}{using}{}{filter}{}{with};",
            lists.clause(" with xmlnamespaces ", &name, "namespaces"),
            lists.clause(" for ", &name, "paths"),
            lists.clause(" include ", &name, "include"),
            lists.clause(" order ", &name, "order"),
            spatial_options(&spatial)
        ));
    }
    let mut ddl = format!("create table {object} (\n    {}\n)", lines.join(",\n    "));
    if let Some(durability) = durability {
        ddl.push_str(&format!(
            " with (memory_optimized = on, durability = {durability})"
        ));
    }
    ddl.push(';');
    for index in indexes {
        ddl.push_str(&format!("\n\n{index}"));
    }
    Ok(ddl)
}

/// Each index's options as ` with (…)`, by the index's name: a hash
/// index's bucket count (#302).
fn index_options(ms: &mut Ms<'_>, object: &str) -> Result<HashMap<String, String>, DbError> {
    let rows = ms.rows(
        "select i.name, cast(h.bucket_count as varchar(20)) \
           from sys.indexes i \
           left join sys.hash_indexes h on h.object_id = i.object_id and h.index_id = i.index_id \
          where i.object_id = object_id(@P1) and i.name is not null",
        &[object],
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter();
            let name = values.next().flatten()?;
            let mut options = Vec::new();
            if let Some(buckets) = values.next().flatten() {
                options.push(format!("bucket_count = {buckets}"));
            }
            (!options.is_empty()).then(|| (name, format!(" with ({})", options.join(", "))))
        })
        .collect())
}

/// `USING … WITH (…)` of a spatial index from its tessellation: the scheme,
/// the bounding box, the grids' four levels and the cells per object, each
/// where it has one; nothing for other indexes.
fn spatial_options(values: &[Cell]) -> String {
    let [scheme, xmin, ymin, xmax, ymax, l1, l2, l3, l4, cells] = values else {
        return String::new();
    };
    let Some(scheme) = scheme else {
        return String::new();
    };
    let mut options = Vec::new();
    if let (Some(xmin), Some(ymin), Some(xmax), Some(ymax)) = (xmin, ymin, xmax, ymax) {
        options.push(format!("bounding_box = ({xmin}, {ymin}, {xmax}, {ymax})"));
    }
    if let (Some(l1), Some(l2), Some(l3), Some(l4)) = (l1, l2, l3, l4) {
        options.push(format!(
            "grids = (level_1 = {l1}, level_2 = {l2}, level_3 = {l3}, level_4 = {l4})"
        ));
    }
    if let Some(cells) = cells {
        options.push(format!("cells_per_object = {cells}"));
    }
    if options.is_empty() {
        format!(" using {scheme}")
    } else {
        format!(" using {scheme} with ({})", options.join(", "))
    }
}

/// The plan of `sql` from `SET SHOWPLAN_ALL`: the server compiles the
/// statement without running it and returns one row per operator. Bind
/// variables become variables of unknown value.
fn explain_plan(ms: &mut Ms<'_>, sql: &str) -> Result<MetaResult, DbError> {
    let sql = binds_as_variables(sql);
    ms.execute("set showplan_all on")?;
    let plan = ms.query(&sql, &[]);
    let off = ms.execute("set showplan_all off");
    let (columns, rows) = plan?;
    off?;
    let index = |name: &str| {
        columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    };
    let get = |row: &[Cell], name: &str| index(name).and_then(|i| row.get(i).cloned().flatten());
    // Steps by (statement, node id).
    let mut ids: Vec<((String, String), usize)> = Vec::new();
    let mut steps: Vec<PlanStep> = Vec::new();
    let mut statement = None;
    for row in &rows {
        let key = |node: Option<String>| {
            (
                get(row, "StmtId").unwrap_or_default(),
                node.unwrap_or_default(),
            )
        };
        let kind = get(row, "Type").unwrap_or_default();
        if kind != "PLAN_ROW" {
            // The statement itself, above its operators.
            statement = Some(steps.len());
            steps.push(PlanStep {
                operation: kind,
                cost: get(row, "TotalSubtreeCost"),
                rows: get(row, "EstimateRows"),
                ..Default::default()
            });
            continue;
        }
        let parent = match get(row, "Parent").as_deref() {
            Some("0") | None => statement,
            Some(_) => {
                let parent = key(get(row, "Parent"));
                ids.iter()
                    .find(|(k, _)| *k == parent)
                    .map(|(_, i)| *i)
                    .or(statement)
            }
        };
        let physical = get(row, "PhysicalOp").unwrap_or_default();
        let logical = get(row, "LogicalOp").unwrap_or_default();
        let operation = if logical.is_empty() || logical == physical {
            physical
        } else {
            format!("{physical} ({logical})")
        };
        let argument = get(row, "Argument").unwrap_or_default();
        ids.push((key(get(row, "NodeId")), steps.len()));
        steps.push(PlanStep {
            parent,
            depth: parent.map_or(0, |p| steps[p].depth + 1),
            operation,
            object: argument_part(&argument, "OBJECT:").map(|o| object_name(&o)),
            cost: get(row, "TotalSubtreeCost"),
            rows: get(row, "EstimateRows"),
            access: argument_part(&argument, "SEEK:"),
            filter: argument_part(&argument, "WHERE:"),
        });
    }
    Ok(plan_table(steps))
}

/// `:name` binds as T-SQL variables (`@name`, `:1` as `@p1`), declared
/// before the statement.
fn binds_as_variables(sql: &str) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for token in pythia_plsql::tokenize(sql) {
        if token.kind != pythia_plsql::TokenKind::BindVar {
            continue;
        }
        let name = sql[token.span.start + 1..token.span.end].to_lowercase();
        let variable = if name.starts_with(|c: char| c.is_ascii_digit()) {
            format!("@p{name}")
        } else {
            format!("@{name}")
        };
        out.push_str(&sql[last..token.span.start]);
        out.push_str(&variable);
        last = token.span.end;
        if !names.contains(&variable) {
            names.push(variable);
        }
    }
    out.push_str(&sql[last..]);
    if names.is_empty() {
        return out;
    }
    let declared: Vec<String> = names
        .iter()
        .map(|n| format!("{n} nvarchar(4000)"))
        .collect();
    format!("declare {};\n{out}", declared.join(", "))
}

/// The parenthesized part after `key` in a SHOWPLAN argument, e.g. the
/// object of `OBJECT:([db].[dbo].[emp].[pk] AS [e]), SEEK:(…)`.
fn argument_part(argument: &str, key: &str) -> Option<String> {
    let start = argument.find(key)? + key.len();
    let rest = argument[start..].strip_prefix('(')?;
    let mut depth = 1;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[..i].to_owned());
                }
            }
            _ => {}
        }
    }
    Some(rest.to_owned())
}

/// `[db].[dbo].[emp].[pk_emp] AS [e]` → `dbo.emp (pk_emp) e`.
fn object_name(object: &str) -> String {
    let (path, alias) = match object.split_once(" AS ") {
        Some((path, alias)) => (path, Some(alias.trim_matches(['[', ']']))),
        None => (object, None),
    };
    let parts: Vec<&str> = path
        .split("].[")
        .map(|p| p.trim_matches(['[', ']']))
        .collect();
    let mut name = match parts.as_slice() {
        [_, schema, table, index] => format!("{schema}.{table} ({index})"),
        [_, schema, table] => format!("{schema}.{table}"),
        _ => path.to_owned(),
    };
    if let Some(alias) = alias {
        name.push(' ');
        name.push_str(alias);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_arguments() {
        let argument = "OBJECT:([pythia].[dbo].[emp].[PK_emp] AS [e]), \
                        SEEK:([e].[id]=(1)) ORDERED FORWARD, WHERE:([e].[sal]>(100))";
        let object = argument_part(argument, "OBJECT:").unwrap();
        assert_eq!(object_name(&object), "dbo.emp (PK_emp) e");
        assert_eq!(
            argument_part(argument, "SEEK:").as_deref(),
            Some("[e].[id]=(1)")
        );
        assert_eq!(
            argument_part(argument, "WHERE:").as_deref(),
            Some("[e].[sal]>(100)")
        );
        assert_eq!(object_name("[pythia].[dbo].[dept]"), "dbo.dept");
        assert_eq!(
            binds_as_variables("select * from t where a = :X and b = ':y' and c = :1 or d = :x"),
            "declare @x nvarchar(4000), @p1 nvarchar(4000);\n\
             select * from t where a = @x and b = ':y' and c = @p1 or d = @x"
        );
    }
}
