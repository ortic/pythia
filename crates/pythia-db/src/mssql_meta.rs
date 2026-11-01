//! SQL Server's answers to [`MetaQuery`]s, from the catalog views of the
//! connection's database. Schemas are the database's schemas; names are
//! stored as written and usually compared without case. Tables have no DDL
//! of their own, so it is put together from the catalog; routines, views
//! and triggers keep their source as written.

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
        // Every column of a columnstore index (types 5 and 6) counts as
        // included: they are its columns, not INCLUDE columns (#252).
        Detail::Indexes => {
            "select i.name as INDEX_NAME, i.type_desc as INDEX_TYPE, \
                    case when i.is_primary_key = 1 then 'PRIMARY' when i.is_unique = 1 then 'UNIQUE' \
                         else 'NONUNIQUE' end as UNIQUENESS, \
                    (select string_agg(col_name(ic.object_id, ic.column_id) \
                              + case when ic.is_descending_key = 1 then ' DESC' else '' end, ', ') \
                              within group (order by ic.key_ordinal, ic.index_column_id) \
                       from sys.index_columns ic \
                      where ic.object_id = i.object_id and ic.index_id = i.index_id \
                        and (ic.is_included_column = 0 or i.type in (5, 6))) as COLUMNS, \
                    (select string_agg(col_name(ic.object_id, ic.column_id), ', ') \
                              within group (order by ic.index_column_id) \
                       from sys.index_columns ic \
                      where ic.object_id = i.object_id and ic.index_id = i.index_id \
                        and ic.is_included_column = 1 and i.type not in (5, 6)) as [INCLUDE], \
                    i.filter_definition as [WHERE] \
               from sys.indexes i where i.object_id = object_id(@P1) and i.type > 0 \
              order by i.is_primary_key desc, i.name"
        }
        Detail::Constraints => {
            "select name as CONSTRAINT_NAME, type as TYPE, definition as DEFINITION, \
                    ref as [REFERENCES] \
               from (select k.name, case k.type when 'PK' then 'Primary key' else 'Unique' end \
                            as type, \
                            (select string_agg(col_name(ic.object_id, ic.column_id), ', ') \
                                      within group (order by ic.key_ordinal) \
                               from sys.index_columns ic \
                              where ic.object_id = k.parent_object_id \
                                and ic.index_id = k.unique_index_id) as definition, \
                            null as ref, case k.type when 'PK' then 1 else 2 end as ord \
                       from sys.key_constraints k where k.parent_object_id = object_id(@P1) \
                     union all \
                     select f.name, 'Foreign key', \
                            (select string_agg(col_name(fc.parent_object_id, fc.parent_column_id), ', ') \
                                      within group (order by fc.constraint_column_id) \
                               from sys.foreign_key_columns fc \
                              where fc.constraint_object_id = f.object_id), \
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
            "select t.name as TRIGGER_NAME, \
                    (select string_agg(e.type_desc, ', ') from sys.trigger_events e \
                      where e.object_id = t.object_id) as TRIGGERING_EVENT, \
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
    let (columns, rows) = ms.query(sql, &[&bracketed(owner, name)])?;
    Ok(MetaResult::Table { columns, rows })
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

/// `CREATE TABLE` with its columns and constraints, then its other indexes.
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
    let constraints = ms.rows(
        "select line from ( \
           select 'constraint ' + quotename(k.name) \
                  + case k.type when 'PK' then ' primary key ' else ' unique ' end \
                  + lower(i.type_desc collate database_default) + ' (' \
                  + (select string_agg(quotename(col_name(ic.object_id, ic.column_id)) \
                              + case when ic.is_descending_key = 1 then ' desc' else '' end, ', ') \
                              within group (order by ic.key_ordinal) \
                       from sys.index_columns ic \
                      where ic.object_id = i.object_id and ic.index_id = i.index_id \
                        and ic.is_included_column = 0) + ')' as line, \
                  case k.type when 'PK' then 1 else 2 end as ord, k.name \
             from sys.key_constraints k \
             join sys.indexes i on i.object_id = k.parent_object_id and i.index_id = k.unique_index_id \
            where k.parent_object_id = object_id(@P1) \
           union all \
           select 'constraint ' + quotename(f.name) + ' foreign key (' \
                  + (select string_agg(quotename(col_name(fc.parent_object_id, fc.parent_column_id)), ', ') \
                              within group (order by fc.constraint_column_id) \
                       from sys.foreign_key_columns fc where fc.constraint_object_id = f.object_id) \
                  + ') references ' + quotename(object_schema_name(f.referenced_object_id)) + '.' \
                  + quotename(object_name(f.referenced_object_id)) + ' (' \
                  + (select string_agg(quotename(col_name(fc.referenced_object_id, \
                                                          fc.referenced_column_id)), ', ') \
                              within group (order by fc.constraint_column_id) \
                       from sys.foreign_key_columns fc where fc.constraint_object_id = f.object_id) \
                  + ')' \
                  + case f.delete_referential_action when 1 then ' on delete cascade' \
                         when 2 then ' on delete set null' when 3 then ' on delete set default' \
                         else '' end \
                  + case f.update_referential_action when 1 then ' on update cascade' \
                         when 2 then ' on update set null' when 3 then ' on update set default' \
                         else '' end, 3, f.name \
             from sys.foreign_keys f where f.parent_object_id = object_id(@P1) \
           union all \
           select 'constraint ' + quotename(c.name) + ' check ' + c.definition, 4, c.name \
             from sys.check_constraints c where c.parent_object_id = object_id(@P1) \
         ) x order by ord, name",
        &[object],
    )?;
    let lines: Vec<String> = columns
        .into_iter()
        .chain(constraints)
        .filter_map(first)
        .collect();
    let mut ddl = format!("create table {object} (\n    {}\n);", lines.join(",\n    "));
    // Rowstore (types 1 and 2) and columnstore indexes (5 and 6, #252). A
    // nonclustered columnstore index names its columns, which count as
    // included; a clustered one holds all of them and names none.
    let indexes = ms.rows(
        "select 'create ' + case when i.is_unique = 1 then 'unique ' else '' end \
                + lower(i.type_desc collate database_default) + ' index ' + quotename(i.name) + ' on ' + @P1 \
                + case when i.type = 5 then '' else ' (' \
                    + (select string_agg(quotename(col_name(ic.object_id, ic.column_id)) \
                                + case when ic.is_descending_key = 1 then ' desc' else '' end, ', ') \
                                within group (order by ic.key_ordinal, ic.index_column_id) \
                         from sys.index_columns ic \
                        where ic.object_id = i.object_id and ic.index_id = i.index_id \
                          and (ic.is_included_column = 0 or i.type = 6)) + ')' end \
                + coalesce(' include (' \
                    + (select string_agg(quotename(col_name(ic.object_id, ic.column_id)), ', ') \
                                within group (order by ic.index_column_id) \
                         from sys.index_columns ic \
                        where ic.object_id = i.object_id and ic.index_id = i.index_id \
                          and ic.is_included_column = 1 and i.type = 2) + ')', '') \
                + coalesce(' where ' + i.filter_definition, '') + ';' \
           from sys.indexes i \
          where i.object_id = object_id(@P1) and i.type in (1, 2, 5, 6) \
            and i.is_primary_key = 0 and i.is_unique_constraint = 0 \
          order by i.name",
        &[object],
    )?;
    for index in indexes.into_iter().filter_map(first) {
        ddl.push_str(&format!("\n\n{index}"));
    }
    Ok(ddl)
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
