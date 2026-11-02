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

/// Whether `sys.index_columns ic` is a column the index was made with: on a
/// partition scheme, an index has the partitioning column too, neither a
/// key nor an included column, unless it was given (#315).
const INDEX_COLUMN: &str = "not (ic.key_ordinal = 0 and ic.is_included_column = 0 \
                                 and ic.partition_ordinal > 0)";

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
        // Whether an index is disabled or hypothetical, which the DDL makes
        // disabled or leaves out (#368).
        Detail::Indexes => {
            "select i.name as INDEX_NAME, i.type_desc as INDEX_TYPE, \
                    case when i.is_primary_key = 1 then 'PRIMARY' when i.is_unique = 1 then 'UNIQUE' \
                         else 'NONUNIQUE' end as UNIQUENESS, \
                    cast(null as nvarchar(4000)) as COLUMNS, \
                    cast(null as nvarchar(4000)) as [INCLUDE], \
                    i.filter_definition as [WHERE], \
                    case when i.is_hypothetical = 1 then 'HYPOTHETICAL' \
                         when i.is_disabled = 1 then 'DISABLED' else 'ENABLED' end as STATUS \
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
    let (lists, filled): (String, &[(usize, &str)]) = match detail {
        Detail::Indexes => (
            format!(
                "select i.name, case when ic.is_included_column = 0 or i.type in (5, 6) \
                                 then 'columns' else 'include' end, \
                    col_name(ic.object_id, ic.column_id) \
                      + case when ic.is_descending_key = 1 then ' DESC' else '' end \
               from sys.indexes i \
               join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
              where i.object_id = object_id(@P1) and i.type > 0 and {INDEX_COLUMN} \
              order by ic.key_ordinal, ic.index_column_id"
            ),
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
              order by 4"
                .to_owned(),
            &[(2, "columns")],
        ),
        Detail::Triggers => (
            "select t.name, 'events', e.type_desc \
               from sys.triggers t join sys.trigger_events e on e.object_id = t.object_id \
              where t.parent_id = object_id(@P1) \
              order by e.type"
                .to_owned(),
            &[(1, "events")],
        ),
        _ => (String::new(), &[]),
    };
    if !filled.is_empty() {
        let lists = Lists::read(ms, &lists, &object)?;
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

/// `CREATE TABLE` with its columns and constraints, then its other indexes
/// and its statistics (#357); a memory-optimized table declares its indexes
/// in it. A column's collation is written where it isn't the database's
/// (#323), so that run elsewhere the others take that database's, as they
/// were made. A temporal table's period columns are `generated always`,
/// hidden or not, and its period comes last (#324). Sparse columns and a
/// column set are declared so (#325), masked columns with their masks
/// (#348): their functions keep their quotes doubled in the catalog, as
/// they go between quotes. A typed XML column names its schema collection,
/// which is made apart, as a foreign key's table is (#349). ROWGUIDCOL, a
/// persisted computed column's NOT NULL and NOT FOR REPLICATION are kept
/// (#355). A graph's node or edge table is made one, without the columns
/// SQL Server adds to it, and with its edge constraints (#360), a ledger
/// table a ledger table, its ledger columns generated always (#366). Always
/// Encrypted columns are encrypted with their key, which is the database's
/// (#367). What CREATE TABLE can't give follows the table: its lock
/// escalation, foreign keys and checks that aren't trusted, disabled ones
/// disabled, and disabled indexes (#326).
fn table_ddl(ms: &mut Ms<'_>, object: &str) -> Result<String, DbError> {
    // Catalog columns of later versions: a columnstore index's ORDER
    // (2022), OPTIMIZE_FOR_SEQUENTIAL_KEY (2019), XML_COMPRESSION (2022), a
    // temporal table's HISTORY_RETENTION_PERIOD (2017), the AUTO_DROP of
    // statistics (2022), graph tables (2017), their edge constraints (2019)
    // and ledger tables (2022).
    let later = ms
        .rows(
            "select col_length('sys.index_columns', 'column_store_order_ordinal'), \
                    col_length('sys.indexes', 'optimize_for_sequential_key'), \
                    col_length('sys.partitions', 'xml_compression'), \
                    col_length('sys.tables', 'history_retention_period'), \
                    col_length('sys.stats', 'auto_drop'), \
                    col_length('sys.tables', 'is_node'), \
                    col_length('sys.edge_constraints', 'object_id'), \
                    col_length('sys.tables', 'ledger_type')",
            &[],
        )?
        .into_iter()
        .next()
        .unwrap_or_default();
    let has = |i: usize| later.get(i).is_some_and(Option::is_some);
    let graph = has(5);
    let graph_type = if graph { "c.graph_type" } else { "null" };
    // Each column and whether SQL Server added it to a graph table: those
    // are left out, AS NODE or AS EDGE adds them (#360).
    let columns = ms.rows(
        &format!(
            "select quotename(c.name) + ' ' \
                    + case when c.is_computed = 1 \
                        then 'as ' + cc.definition \
                             + case when cc.is_persisted = 0 then '' \
                                    when c.is_nullable = 1 then ' persisted' \
                                    else ' persisted not null' end \
                        else {COLUMN_TYPE} \
                             + coalesce('(' + case when c.is_xml_document = 1 then 'document ' \
                                                   else 'content ' end \
                                        + quotename(schema_name(xs.schema_id)) + '.' \
                                        + quotename(xs.name) + ')', '') \
                             + case when c.collation_name collate database_default \
                                         <> cast(databasepropertyex(db_name(), 'Collation') \
                                                 as sysname) collate database_default \
                                 then ' collate ' + c.collation_name else '' end \
                             + coalesce(' encrypted with (column_encryption_key = ' \
                                        + quotename(ek.name) + ', encryption_type = ' \
                                        + lower(c.encryption_type_desc collate database_default) \
                                        + ', algorithm = ''' \
                                        + c.encryption_algorithm_name collate database_default \
                                        + ''')', '') \
                             + case when c.is_sparse = 1 then ' sparse' \
                                    when c.is_column_set = 1 \
                                      then ' column_set for all_sparse_columns' \
                                    else '' end \
                             + coalesce(' masked with (function = ''' \
                                        + m.masking_function collate database_default \
                                        + ''')', '') \
                             + case when ic.object_id is not null \
                                 then ' identity(' + cast(ic.seed_value as varchar(40)) + ', ' \
                                      + cast(ic.increment_value as varchar(40)) + ')' \
                                      + case when ic.is_not_for_replication = 1 \
                                             then ' not for replication' else '' end \
                                 else '' end \
                             + case when c.generated_always_type in (1, 2, 7, 8, 9, 10) \
                                 then ' generated always as ' \
                                      + case c.generated_always_type when 1 then 'row start' \
                                             when 2 then 'row end' \
                                             when 7 then 'transaction_id start' \
                                             when 8 then 'transaction_id end' \
                                             when 9 then 'sequence_number start' \
                                             else 'sequence_number end' end \
                                      + case when c.is_hidden = 1 then ' hidden' else '' end \
                                 else '' end \
                             + case when c.is_nullable = 1 then ' null' else ' not null' end \
                             + case when c.is_rowguidcol = 1 then ' rowguidcol' else '' end \
                             + coalesce(' constraint ' + quotename(dc.name) + ' default ' \
                                        + dc.definition, '') end, \
                    {graph_type} \
               from sys.columns c join sys.types t on t.user_type_id = c.user_type_id \
               left join sys.computed_columns cc on cc.object_id = c.object_id \
                and cc.column_id = c.column_id \
               left join sys.identity_columns ic on ic.object_id = c.object_id \
                and ic.column_id = c.column_id \
               left join sys.default_constraints dc on dc.parent_object_id = c.object_id \
                and dc.parent_column_id = c.column_id \
               left join sys.masked_columns m on m.object_id = c.object_id \
                and m.column_id = c.column_id and m.is_masked = 1 \
               left join sys.xml_schema_collections xs on xs.xml_collection_id = c.xml_collection_id \
                and c.xml_collection_id > 0 \
               left join sys.column_encryption_keys ek \
                 on ek.column_encryption_key_id = c.column_encryption_key_id \
              where c.object_id = object_id(@P1) \
              order by c.column_id"
        ),
        &[object],
    )?;
    if columns.is_empty() {
        return Err(DbError::new(format!("No table {object}")));
    }
    let node_or_edge = if graph {
        "case when t.is_node = 1 then 'node' when t.is_edge = 1 then 'edge' end"
    } else {
        "null"
    };
    // A ledger table's kind, updatable (2) or append-only (3), its ledger
    // view and whether that isn't the table's name and _Ledger in its
    // schema (#366).
    let ledger = if has(7) {
        "t.ledger_type, \
         quotename(object_schema_name(t.ledger_view_id)) + '.' \
           + quotename(object_name(t.ledger_view_id)), \
         case when object_schema_name(t.ledger_view_id) <> schema_name(t.schema_id) \
                or object_name(t.ledger_view_id) <> t.name + '_Ledger' then 1 end"
    } else {
        "null, null, null"
    };
    let retention = if has(3) {
        "case when t.history_retention_period > 0 \
              then cast(t.history_retention_period as varchar(10)) + ' ' \
                   + lower(t.history_retention_period_unit_desc collate database_default) \
                   + case when t.history_retention_period > 1 then 's' else '' end end"
    } else {
        "null"
    };
    // A memory-optimized table's durability (#302): CREATE INDEX can't make
    // its indexes, they are declared in CREATE TABLE. Its LOB data's
    // filegroup where it isn't the table's, and whether the table is a heap
    // (0) or its clustered index (1) holds its rows (#315). A temporal
    // table's period, whether it is system-versioned (2), its history table
    // where SQL Server didn't name it, MSSQL_TemporalHistoryFor_ and the
    // table's object_id in its schema, so that a copy gets its own, and the
    // history's retention where it isn't infinite (#324). Its lock
    // escalation where it isn't TABLE (#326). Whether it is a graph's node
    // or edge table (#360). A ledger table's history is named so too, with
    // MSSQL_LedgerHistoryFor_ (#366).
    let table = ms
        .rows(
            &format!(
                "select case when t.is_memory_optimized = 1 \
                             then lower(t.durability_desc collate database_default) end, \
                        case when t.lob_data_space_id not in (0, i.data_space_id) \
                             then quotename(l.name) end, \
                        i.index_id, \
                        'period for system_time (' \
                          + quotename(col_name(p.object_id, p.start_column_id)) + ', ' \
                          + quotename(col_name(p.object_id, p.end_column_id)) + ')', \
                        t.temporal_type, \
                        case when h.schema_id <> t.schema_id \
                               or h.name <> case when t.temporal_type = 2 \
                                                 then 'MSSQL_TemporalHistoryFor_' \
                                                 else 'MSSQL_LedgerHistoryFor_' end \
                                            + cast(t.object_id as varchar(20)) \
                             then quotename(schema_name(h.schema_id)) + '.' + quotename(h.name) \
                        end, \
                        {retention}, \
                        case when t.lock_escalation <> 0 and t.is_memory_optimized = 0 \
                             then lower(t.lock_escalation_desc collate database_default) end, \
                        {node_or_edge}, {ledger} \
                   from sys.tables t \
                   join sys.indexes i on i.object_id = t.object_id and i.index_id in (0, 1) \
                   left join sys.data_spaces l on l.data_space_id = t.lob_data_space_id \
                   left join sys.periods p on p.object_id = t.object_id and p.period_type = 1 \
                   left join sys.tables h on h.object_id = t.history_table_id \
                  where t.object_id = object_id(@P1)"
            ),
            &[object],
        )?
        .into_iter()
        .next()
        .unwrap_or_default();
    let mut table = table.into_iter();
    let mut next = || table.next().flatten();
    let (durability, textimage, rows_in) = (next(), next(), next().unwrap_or_default());
    let (period, versioned) = (next(), next().as_deref() == Some("2"));
    let (history, retention, escalation) = (next(), next(), next());
    let node_or_edge = next();
    let (ledger, ledger_view, view_named) = (next(), next(), next().is_some());
    let ledger = ledger.filter(|kind| kind == "2" || kind == "3");
    let storage = index_storage(ms, object, has(1), has(2))?;
    let no_storage = Storage::default();
    let storage_of = |index: &str| storage.get(index).unwrap_or(&no_storage);
    let table_storage = storage_of(&rows_in);
    // A key's columns, a foreign key's and the ones it references, and an
    // edge constraint's clauses (#360), quoted, as lists (see Lists).
    let key_column = column_of("ic", graph);
    let clauses = if has(6) {
        "union all \
         select e.name, 'columns', \
                quotename(object_schema_name(c.from_object_id)) + '.' \
                  + quotename(object_name(c.from_object_id)) + ' to ' \
                  + quotename(object_schema_name(c.to_object_id)) + '.' \
                  + quotename(object_name(c.to_object_id)), \
                c.clause_number \
           from sys.edge_constraints e \
           join sys.edge_constraint_clauses c on c.object_id = e.object_id \
          where e.parent_object_id = object_id(@P1)"
    } else {
        ""
    };
    let lists = Lists::read(
        ms,
        &format!(
            "select k.name, 'columns', {key_column} \
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
         {clauses} \
          order by 4"
        ),
        object,
    )?;
    // Each constraint as (name, its line up to the columns, what follows
    // them: a foreign key's REFERENCES and its actions, a key's index), and
    // whether a foreign key or check isn't trusted (1), as WITH NOCHECK
    // makes it, or is disabled too (2) (#326). One NOT FOR REPLICATION is
    // never trusted, however it is made (#355). Edge constraints follow
    // the checks (#360).
    let edges = if has(6) {
        "union all \
         select e.name, 'constraint ' + quotename(e.name) + ' connection', null, \
                case when e.delete_referential_action = 1 then ' on delete cascade' else '' end, \
                null, \
                case when e.is_disabled = 1 then 2 when e.is_not_trusted = 1 then 1 else 0 end, \
                5 \
           from sys.edge_constraints e where e.parent_object_id = object_id(@P1)"
    } else {
        ""
    };
    let constraints = ms.rows(
        &format!(
            "select name, line, refs, actions, ix, untrusted, quotename(name) from ( \
           select k.name, 'constraint ' + quotename(k.name) \
                  + case k.type when 'PK' then ' primary key ' else ' unique ' end \
                  + lower(i.type_desc collate database_default) as line, \
                  null as refs, '' as actions, i.index_id as ix, 0 as untrusted, \
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
                         else '' end \
                  + case when f.is_not_for_replication = 1 then ' not for replication' \
                         else '' end, null, \
                  case when f.is_disabled = 1 then 2 \
                       when f.is_not_trusted = 1 and f.is_not_for_replication = 0 then 1 \
                       when f.referenced_object_id = f.parent_object_id \
                        and not exists (select 1 from sys.key_constraints kc \
                                         where kc.parent_object_id = f.referenced_object_id \
                                           and kc.unique_index_id = f.key_index_id) then 3 \
                       else 0 end, \
                  3 \
             from sys.foreign_keys f where f.parent_object_id = object_id(@P1) \
           union all \
           select c.name, 'constraint ' + quotename(c.name) + ' check ' \
                  + case when c.is_not_for_replication = 1 then 'not for replication ' \
                         else '' end \
                  + c.definition, null, '', null, \
                  case when c.is_disabled = 1 then 2 \
                       when c.is_not_trusted = 1 and c.is_not_for_replication = 0 then 1 \
                       else 0 end, \
                  4 \
             from sys.check_constraints c where c.parent_object_id = object_id(@P1) \
           {edges} \
         ) x order by ord, name"
        ),
        &[object],
    )?;
    let mut lines: Vec<String> = columns
        .into_iter()
        .filter(|row| !row.get(1).is_some_and(Option::is_some))
        .filter_map(first)
        .collect();
    // The constraints that aren't trusted are added after the table and its
    // indexes, WITH NOCHECK, and the disabled ones disabled. So is a foreign
    // key to the table's own unique index that no constraint makes, which
    // the create can't have yet.
    let mut nocheck = Vec::new();
    for row in constraints {
        let mut values = row.into_iter();
        let mut next = || values.next().flatten();
        let (Some(name), Some(line)) = (next(), next()) else {
            continue;
        };
        let refs = next();
        let (actions, index) = (next().unwrap_or_default(), next());
        let (state, quoted) = (next(), next().unwrap_or_default());
        let columns = lists.clause(" ", &name, "columns");
        let references = refs
            .map(|refs| format!("{refs}{}", lists.clause(" ", &name, "references")))
            .unwrap_or_default();
        let storage = index.map(|index| storage_of(&index).clauses(&[], table_storage));
        let constraint = format!(
            "{line}{columns}{references}{actions}{}",
            storage.unwrap_or_default()
        );
        match state.as_deref() {
            Some(state @ ("1" | "2")) => {
                nocheck.push(format!(
                    "alter table {object} with nocheck add {constraint};"
                ));
                if state == "2" {
                    nocheck.push(format!("alter table {object} nocheck constraint {quoted};"));
                }
            }
            Some("3") => nocheck.push(format!("alter table {object} add {constraint};")),
            _ => lines.push(constraint),
        }
    }
    let mut indexes = Vec::new();
    // Rowstore (types 1 and 2) and columnstore indexes (5 and 6, #252). A
    // nonclustered columnstore index names its columns, which count as
    // included; a clustered one holds all of them and names none. XML (3)
    // and spatial indexes (4) follow, a secondary XML index after the
    // primary one it uses, and a columnstore index's ORDER, which SQL
    // Server has from 2022 (#265). A selective XML index has its
    // namespaces and paths, a secondary one the path it is on (#311).
    let order = if has(0) {
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
                    {key_column} \
                      + case when ic.is_descending_key = 1 then ' desc' else '' end, \
                    ic.key_ordinal, ic.index_column_id \
               from sys.indexes i \
               join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
              where i.object_id = object_id(@P1) and {INDEX_COLUMN} \
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
    // indexes (7) are a memory-optimized table's (#302). The clustered
    // index, rowstore or columnstore, comes first, so that the others
    // aren't built again over it; hypothetical indexes, which have
    // statistics and no rows, are left out (#327), and so is a graph
    // table's own unique index (#360).
    let own_index = own_index(graph);
    let rows = ms.rows(
        &format!(
            "select i.index_id, i.name, quotename(i.name), \
                case when i.is_unique = 1 then 'unique ' else '' end, \
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
            and i.is_primary_key = 0 and i.is_unique_constraint = 0 and i.is_hypothetical = 0 \
            {own_index} \
          order by case when i.type in (1, 5) then 0 when x.using_xml_index_id is null then 1 \
                        else 2 end, \
                   i.name"
        ),
        &[object],
    )?;
    for row in rows {
        let mut values = row.into_iter();
        let mut next = || values.next().flatten();
        let (Some(id), Some(name), Some(quoted)) = (next(), next(), next()) else {
            continue;
        };
        let kind = format!(
            "{}{}",
            next().unwrap_or_default(),
            next().unwrap_or_default()
        );
        let (using, filter) = (next().unwrap_or_default(), next().unwrap_or_default());
        let spatial: Vec<Cell> = values.collect();
        let (scheme, spatial) = spatial_options(&spatial);
        let columns = lists.clause(" ", &name, "columns");
        let storage = storage_of(&id).clauses(&spatial, table_storage);
        if durability.is_some() {
            lines.push(format!("index {quoted} {kind}{columns}{storage}"));
            continue;
        }
        indexes.push(format!(
            "create {kind} index {quoted} on {object}{columns}{}{}{}{using}{}{filter}{scheme}\
             {storage};",
            lists.clause(" with xmlnamespaces ", &name, "namespaces"),
            lists.clause(" for ", &name, "paths"),
            lists.clause(" include ", &name, "include"),
            lists.clause(" order ", &name, "order"),
        ));
    }
    lines.extend(period);
    // The table's filegroup or partition scheme, and its LOB data's, then
    // its options: a memory-optimized table's, a heap's compression, a
    // temporal table's versioning. An edge table may have no columns of its
    // own (#360).
    let mut ddl = if lines.is_empty() {
        format!("create table {object}")
    } else {
        format!("create table {object} (\n    {}\n)", lines.join(",\n    "))
    };
    if let Some(node_or_edge) = node_or_edge {
        ddl.push_str(&format!(" as {node_or_edge}"));
    }
    if let Some(on) = table_storage.on.as_ref().filter(|_| !table_storage.default) {
        ddl.push_str(&format!(" on {on}"));
    }
    if let Some(textimage) = textimage {
        ddl.push_str(&format!(" textimage_on {textimage}"));
    }
    let mut options = Vec::new();
    if let Some(durability) = durability {
        options.push("memory_optimized = on".to_owned());
        options.push(format!("durability = {durability}"));
    }
    if rows_in == "0" {
        options.extend(table_storage.options.iter().cloned());
    }
    // An updatable ledger table is system-versioned, without a period
    // (#366).
    if versioned || ledger.as_deref() == Some("2") {
        let history: Vec<String> = [
            history.map(|history| format!("history_table = {history}")),
            retention.map(|retention| format!("history_retention_period = {retention}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        options.push(if history.is_empty() {
            "system_versioning = on".to_owned()
        } else {
            format!("system_versioning = on ({})", history.join(", "))
        });
    }
    if let Some(ledger) = ledger {
        options.push(ledger_option(ms, object, &ledger, ledger_view, view_named)?);
    }
    if !options.is_empty() {
        ddl.push_str(&format!(" with ({})", options.join(", ")));
    }
    ddl.push(';');
    if let Some(escalation) = escalation {
        ddl.push_str(&format!(
            "\n\nalter table {object} set (lock_escalation = {escalation});"
        ));
    }
    // Disabled indexes, keys' too, are made and then disabled, last of all
    // and a clustered one after the others: once it is disabled, the table
    // takes no more indexes or constraints (#326).
    let disabled = ms
        .rows(
            &format!(
                "select quotename(i.name) from sys.indexes i \
                  where i.object_id = object_id(@P1) and i.is_disabled = 1 \
                    and i.is_hypothetical = 0 {own_index} \
                  order by case when i.type in (1, 5) then 1 else 0 end, i.name"
            ),
            &[object],
        )?
        .into_iter()
        .filter_map(first)
        .map(|index| format!("alter index {index} on {object} disable;"));
    let statistics = statistics(ms, object, has(4), graph)?;
    for statement in indexes
        .into_iter()
        .chain(statistics)
        .chain(nocheck)
        .chain(disabled)
    {
        ddl.push_str(&format!("\n\n{statement}"));
    }
    Ok(ddl)
}

/// `ledger = on` of a ledger table of `kind`, updatable (2) or append-only
/// (3), with its ledger `view` where it is `named` other than the default,
/// or where the view's columns are, as they then follow it (#366).
fn ledger_option(
    ms: &mut Ms<'_>,
    object: &str,
    kind: &str,
    view: Option<String>,
    named: bool,
) -> Result<String, DbError> {
    let columns: Vec<String> = ms
        .rows(
            "select case c.ledger_view_column_type \
                      when 1 then 'transaction_id_column_name' \
                      when 2 then 'sequence_number_column_name' \
                      when 3 then 'operation_type_column_name' \
                      else 'operation_type_desc_column_name' end \
                    + ' = ' + quotename(c.name) \
               from sys.tables t join sys.columns c on c.object_id = t.ledger_view_id \
              where t.object_id = object_id(@P1) and c.ledger_view_column_type in (1, 2, 3, 4) \
                and c.name <> case c.ledger_view_column_type \
                                when 1 then 'ledger_transaction_id' \
                                when 2 then 'ledger_sequence_number' \
                                when 3 then 'ledger_operation_type' \
                                else 'ledger_operation_type_desc' end \
              order by c.ledger_view_column_type",
            &[object],
        )?
        .into_iter()
        .filter_map(first)
        .collect();
    let mut options = Vec::new();
    if let Some(view) = view.filter(|_| named || !columns.is_empty()) {
        let columns = if columns.is_empty() {
            String::new()
        } else {
            format!(" ({})", columns.join(", "))
        };
        options.push(format!("ledger_view = {view}{columns}"));
    }
    if kind == "3" {
        options.push("append_only = on".to_owned());
    }
    Ok(if options.is_empty() {
        "ledger = on".to_owned()
    } else {
        format!("ledger = on ({})", options.join(", "))
    })
}

/// The statistics made on the table, after its indexes, with their filters
/// and the options that aren't the defaults (#357); those of its indexes,
/// and the ones SQL Server made itself, aren't. `auto_drop` tells whether
/// the catalog has AUTO_DROP (2022), `graph` graph tables (2017). A
/// persisted sample percent isn't kept by CREATE STATISTICS on a table
/// without rows.
fn statistics(
    ms: &mut Ms<'_>,
    object: &str,
    auto_drop: bool,
    graph: bool,
) -> Result<Vec<String>, DbError> {
    let column = column_of("sc", graph);
    let columns = Lists::read(
        ms,
        &format!(
            "select s.name, 'columns', {column}, sc.stats_column_id \
               from sys.stats s \
               join sys.stats_columns sc on sc.object_id = s.object_id \
                and sc.stats_id = s.stats_id \
              where s.object_id = object_id(@P1) and s.user_created = 1 \
              order by 4"
        ),
        object,
    )?;
    let auto_drop = if auto_drop { "s.auto_drop" } else { "0" };
    let rows = ms.rows(
        &format!(
            "select s.name, quotename(s.name), coalesce(' where ' + s.filter_definition, ''), \
                    s.no_recompute, s.is_incremental, {auto_drop} \
               from sys.stats s \
              where s.object_id = object_id(@P1) and s.user_created = 1 and s.is_temporary = 0 \
              order by s.name"
        ),
        &[object],
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let get = |i: usize| row.get(i).cloned().flatten();
            let on = |i: usize| get(i).as_deref() == Some("1");
            let (name, quoted) = (get(0)?, get(1)?);
            let options: Vec<&str> = [
                (on(3), "norecompute"),
                (on(4), "incremental = on"),
                (on(5), "auto_drop = on"),
            ]
            .into_iter()
            .filter_map(|(on, option)| on.then_some(option))
            .collect();
            let with = if options.is_empty() {
                String::new()
            } else {
                format!(" with {}", options.join(", "))
            };
            Some(format!(
                "create statistics {quoted} on {object}{}{}{with};",
                columns.clause(" ", &name, "columns"),
                get(2).unwrap_or_default()
            ))
        })
        .collect())
}

/// The column of `columns` (`sys.index_columns` or `sys.stats_columns`)
/// quoted, as an index or statistics are made on it. On a graph table
/// (`graph`: the catalog has them, from 2017), the columns SQL Server added
/// are named as their pseudo-column, `$node_id` or `$edge_id`, and
/// `$from_id` and `$to_id` for the pairs they stand for (#360): the second
/// of a pair is NULL, which Lists leaves out.
fn column_of(columns: &str, graph: bool) -> String {
    if !graph {
        return format!("quotename(col_name({columns}.object_id, {columns}.column_id))");
    }
    format!(
        "(select case gc.graph_type \
                   when 1 then case when gt.is_edge = 1 then '$edge_id' else '$node_id' end \
                   when 4 then '$from_id' when 7 then '$to_id' \
                   when 3 then null when 6 then null \
                   else quotename(gc.name) end \
            from sys.columns gc join sys.tables gt on gt.object_id = gc.object_id \
           where gc.object_id = {columns}.object_id and gc.column_id = {columns}.column_id)"
    )
}

/// A condition that leaves out the unique index SQL Server makes on a node
/// or edge table's `$node_id` or `$edge_id`, `GRAPH_UNIQUE_INDEX_…`, where
/// no key is on it: AS NODE or AS EDGE makes it again (#360).
fn own_index(graph: bool) -> &'static str {
    if graph {
        "and not (i.name like 'GRAPH[_]UNIQUE[_]INDEX[_]%' \
                  and i.object_id in (select g.object_id from sys.tables g \
                                       where g.is_node = 1 or g.is_edge = 1))"
    } else {
        ""
    }
}

/// What an index has that isn't the default: its options, for `with (…)`,
/// and the filegroup or partition scheme it is on (`[ps] ([column])`), and
/// whether that is the database's default filegroup.
#[derive(Default)]
struct Storage {
    options: Vec<String>,
    on: Option<String>,
    default: bool,
}

impl Storage {
    /// ` with (…)`, `first` before the index's options, and ` on …` where
    /// the index isn't on `table`'s filegroup or partition scheme, where
    /// CREATE INDEX puts it.
    fn clauses(&self, first: &[String], table: &Storage) -> String {
        let options: Vec<&str> = first
            .iter()
            .chain(&self.options)
            .map(String::as_str)
            .collect();
        let mut clauses = String::new();
        if !options.is_empty() {
            clauses.push_str(&format!(" with ({})", options.join(", ")));
        }
        if let Some(on) = self.on.as_ref().filter(|&on| table.on.as_ref() != Some(on)) {
            clauses.push_str(&format!(" on {on}"));
        }
        clauses
    }
}

/// Each index's storage, by its index_id: a hash index's bucket count
/// (#302), the options of CREATE INDEX that aren't the defaults, its
/// compression, by partition where they differ, and its filegroup or
/// partition scheme (#315). Columnstore indexes and a memory-optimized
/// table's take no rowstore options (their locks read as off), an XML
/// index no filegroup, a heap only its compression and filegroup.
/// `sequential` and `xml` tell whether the catalog has
/// OPTIMIZE_FOR_SEQUENTIAL_KEY (2019) and XML_COMPRESSION (2022).
fn index_storage(
    ms: &mut Ms<'_>,
    object: &str,
    sequential: bool,
    xml: bool,
) -> Result<HashMap<String, Storage>, DbError> {
    let sequential = if sequential {
        "i.optimize_for_sequential_key"
    } else {
        "0"
    };
    let rows = ms.rows(
        &format!(
            "select i.index_id, i.type, tb.is_memory_optimized, h.bucket_count, i.is_padded, \
                    i.fill_factor, i.ignore_dup_key, st.no_recompute, st.is_incremental, \
                    i.allow_row_locks, i.allow_page_locks, {sequential}, i.compression_delay, \
                    case when i.type <> 3 and i.data_space_id <> 0 \
                         then quotename(d.name) + coalesce(' (' + quotename(pc.name) + ')', '') end, \
                    case when i.data_space_id = (select f.data_space_id from sys.filegroups f \
                                                  where f.is_default = 1 and f.type = 'FG') \
                         then 1 else 0 end \
               from sys.indexes i \
               join sys.tables tb on tb.object_id = i.object_id \
               left join sys.hash_indexes h on h.object_id = i.object_id and h.index_id = i.index_id \
               left join sys.stats st on st.object_id = i.object_id and st.stats_id = i.index_id \
               left join sys.data_spaces d on d.data_space_id = i.data_space_id \
              outer apply (select col_name(ic.object_id, ic.column_id) as name \
                             from sys.index_columns ic \
                            where ic.object_id = i.object_id and ic.index_id = i.index_id \
                              and ic.partition_ordinal = 1) pc \
              where i.object_id = object_id(@P1)"
        ),
        &[object],
    )?;
    let xml = if xml {
        "lower(p.xml_compression_desc)"
    } else {
        "'off'"
    };
    let mut partitions: HashMap<String, Vec<(u32, String, String)>> = HashMap::new();
    for row in ms.rows(
        &format!(
            "select p.index_id, p.partition_number, lower(p.data_compression_desc), {xml} \
               from sys.partitions p where p.object_id = object_id(@P1) \
              order by p.index_id, p.partition_number"
        ),
        &[object],
    )? {
        let mut values = row.into_iter().map(Option::unwrap_or_default);
        let (Some(index), Some(number)) = (values.next(), values.next()) else {
            continue;
        };
        let (data, xml) = (
            values.next().unwrap_or_default(),
            values.next().unwrap_or_default(),
        );
        partitions
            .entry(index)
            .or_default()
            .push((number.parse().unwrap_or_default(), data, xml));
    }
    let mut storage = HashMap::new();
    for row in rows {
        let get = |i: usize| row.get(i).cloned().flatten();
        let on = |i: usize| get(i).as_deref() == Some("1");
        let off = |i: usize| get(i).as_deref() == Some("0");
        let Some(id) = get(0) else {
            continue;
        };
        let kind = get(1).unwrap_or_default();
        let memory_optimized = on(2);
        let columnstore = matches!(kind.as_str(), "5" | "6");
        let mut options = Vec::new();
        if let Some(buckets) = get(3) {
            options.push(format!("bucket_count = {buckets}"));
        }
        if !memory_optimized && !columnstore && kind != "0" {
            if on(4) {
                options.push("pad_index = on".to_owned());
            }
            if let Some(fill) = get(5).filter(|f| f != "0") {
                options.push(format!("fillfactor = {fill}"));
            }
            if on(6) {
                options.push("ignore_dup_key = on".to_owned());
            }
            // An XML index takes no statistics options.
            if on(7) && kind != "3" {
                options.push("statistics_norecompute = on".to_owned());
            }
            if on(8) && kind != "3" {
                options.push("statistics_incremental = on".to_owned());
            }
            if off(9) {
                options.push("allow_row_locks = off".to_owned());
            }
            if off(10) {
                options.push("allow_page_locks = off".to_owned());
            }
            if on(11) {
                options.push("optimize_for_sequential_key = on".to_owned());
            }
        }
        if let Some(delay) = get(12).filter(|d| d != "0") {
            options.push(format!("compression_delay = {delay}"));
        }
        if !memory_optimized && let Some(parts) = partitions.get(&id) {
            let data: Vec<(u32, &str)> = parts.iter().map(|(n, d, _)| (*n, d.as_str())).collect();
            let default = if columnstore { "columnstore" } else { "none" };
            options.extend(by_partition("data_compression", &data, default));
            let xml: Vec<(u32, &str)> = parts.iter().map(|(n, _, x)| (*n, x.as_str())).collect();
            options.extend(by_partition("xml_compression", &xml, "off"));
        }
        storage.insert(
            id,
            Storage {
                options,
                on: get(13),
                default: on(14),
            },
        );
    }
    Ok(storage)
}

/// `name = value` where partitions have a value that isn't `default`: one
/// for all of them alike, else each value `on partitions (…)`.
fn by_partition(name: &str, partitions: &[(u32, &str)], default: &str) -> Vec<String> {
    let Some(&(_, first)) = partitions.first() else {
        return Vec::new();
    };
    if partitions.iter().all(|&(_, value)| value == first) {
        return if first == default {
            Vec::new()
        } else {
            vec![format!("{name} = {first}")]
        };
    }
    let mut values: Vec<&str> = Vec::new();
    for &(_, value) in partitions {
        if value != default && !values.contains(&value) {
            values.push(value);
        }
    }
    values
        .into_iter()
        .map(|value| {
            let numbers: Vec<u32> = partitions
                .iter()
                .filter(|&&(_, v)| v == value)
                .map(|&(n, _)| n)
                .collect();
            format!("{name} = {value} on partitions ({})", ranges(&numbers))
        })
        .collect()
}

/// Partition numbers in order as `1, 3 to 5`.
fn ranges(numbers: &[u32]) -> String {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < numbers.len() {
        let mut j = i;
        while j + 1 < numbers.len() && numbers[j + 1] == numbers[j] + 1 {
            j += 1;
        }
        ranges.push(if j > i {
            format!("{} to {}", numbers[i], numbers[j])
        } else {
            numbers[i].to_string()
        });
        i = j + 1;
    }
    ranges.join(", ")
}

/// ` using …` of a spatial index and its options from its tessellation:
/// the bounding box, the grids' four levels and the cells per object, each
/// where it has one; nothing for other indexes.
fn spatial_options(values: &[Cell]) -> (String, Vec<String>) {
    let [scheme, xmin, ymin, xmax, ymax, l1, l2, l3, l4, cells] = values else {
        return Default::default();
    };
    let Some(scheme) = scheme else {
        return Default::default();
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
    (format!(" using {scheme}"), options)
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
    fn compression_by_partition() {
        assert_eq!(ranges(&[1, 3, 4, 5, 7, 8]), "1, 3 to 5, 7 to 8");
        assert_eq!(ranges(&[2]), "2");
        let none: Vec<String> = Vec::new();
        assert_eq!(
            by_partition("data_compression", &[(1, "none"), (2, "none")], "none"),
            none
        );
        assert_eq!(
            by_partition("data_compression", &[(1, "page"), (2, "page")], "none"),
            ["data_compression = page"]
        );
        assert_eq!(
            by_partition(
                "data_compression",
                &[
                    (1, "page"),
                    (2, "row"),
                    (3, "page"),
                    (4, "none"),
                    (5, "row")
                ],
                "none"
            ),
            [
                "data_compression = page on partitions (1, 3)",
                "data_compression = row on partitions (2, 5)"
            ]
        );
        assert_eq!(
            by_partition(
                "data_compression",
                &[
                    (1, "columnstore"),
                    (2, "columnstore_archive"),
                    (3, "columnstore_archive")
                ],
                "columnstore"
            ),
            ["data_compression = columnstore_archive on partitions (2 to 3)"]
        );
    }

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
