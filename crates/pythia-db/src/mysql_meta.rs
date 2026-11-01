//! MySQL's answers to [`MetaQuery`]s, from `INFORMATION_SCHEMA` and the
//! `SHOW CREATE` statements. Schemas are MySQL's databases; names keep
//! their case, and filters match without it.

use mysql::prelude::Queryable;
use mysql::{Conn, Params, Value};

use crate::dialect::Dialect;
use crate::meta::{
    CompileError, DbObject, Detail, MetaQuery, MetaResult, ObjectType, TableColumn, like_pattern,
};
use crate::mysql_session::{all_rows, column};
use crate::types::{Cell, Column, DbError};

/// `DATE_FORMAT` pattern of [`DbObject::last_ddl`].
const DDL_TIME: &str = "'%Y-%m-%d %H:%i:%s'";

pub(crate) fn run(conn: &mut Conn, query: &MetaQuery) -> Result<MetaResult, DbError> {
    Ok(match query {
        MetaQuery::Schemas => MetaResult::Schemas(
            conn.query("select schema_name from information_schema.schemata order by schema_name")?,
        ),
        MetaQuery::Objects {
            owner,
            object_type,
            filter,
        } => MetaResult::Objects(objects(conn, owner, *object_type, filter.as_deref())?),
        MetaQuery::Columns { owner, table, keys } => {
            MetaResult::Columns(columns(conn, owner, table, *keys)?)
        }
        // Stored routines live on their own, not in packages.
        MetaQuery::Members { .. } => MetaResult::Members(Vec::new()),
        MetaQuery::Details {
            owner,
            name,
            detail,
        } => details(conn, owner, name, *detail)?,
        MetaQuery::Source {
            owner,
            name,
            object_type,
        } => MetaResult::Text(show_create(conn, owner, name, *object_type)?),
        MetaQuery::Ddl {
            owner,
            name,
            object_type,
        } => {
            let ddl = show_create(conn, owner, name, *object_type)?;
            MetaResult::Text(match object_type {
                // Routine and trigger bodies have `;` of their own.
                ObjectType::Table | ObjectType::View => format!("{ddl};"),
                _ => ddl,
            })
        }
        // MySQL reports errors when a routine is created, not afterwards.
        MetaQuery::Errors { .. } => MetaResult::Errors(Vec::<CompileError>::new()),
        MetaQuery::Find { pattern, limit } => MetaResult::Objects(find(conn, pattern, *limit)?),
        MetaQuery::ExplainPlan { sql } => explain_plan(conn, sql)?,
        MetaQuery::TestLink { .. } => {
            return Err(DbError::unsupported("Database links", Dialect::MYSQL));
        }
        MetaQuery::Accounts(object_type) => MetaResult::Accounts(accounts(conn, *object_type)?),
    })
}

/// Users as `user@host`, roles by name (`name@host` for a host of their
/// own), from `mysql.user`: MariaDB marks its roles, MySQL's are the locked
/// accounts without a password. Without access to it, the session's own
/// user and the roles it may use.
fn accounts(conn: &mut Conn, object_type: ObjectType) -> Result<Vec<(String, String)>, DbError> {
    let role = object_type == ObjectType::Role;
    let (mariadb, mysql) = if role {
        (
            "is_role = 'Y'",
            "account_locked = 'Y' and authentication_string = ''",
        )
    } else {
        (
            "is_role = 'N'",
            "not (account_locked = 'Y' and authentication_string = '')",
        )
    };
    let sql = |accounts: &str| format!("select user, host from mysql.user where {accounts}");
    let rows: Result<Vec<(String, String)>, _> = match conn.query(sql(mariadb)) {
        // No is_role: MySQL.
        Err(mysql::Error::MySqlError(e)) if e.code == 1054 => conn.query(sql(mysql)),
        rows => rows,
    };
    let mut names: Vec<String> = match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|(user, host)| match host.as_str() {
                "%" | "" if role => user,
                _ => format!("{user}@{host}"),
            })
            .collect(),
        // No access to mysql.user.
        Err(mysql::Error::MySqlError(e)) if e.code == 1142 => {
            if role {
                conn.query("select distinct role_name from information_schema.applicable_roles")?
            } else {
                conn.query("select current_user()")?
            }
        }
        Err(e) => return Err(e.into()),
    };
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| (name, String::new()))
        .collect())
}

/// One object per row of `(owner, name, type, last change)`.
fn object_rows(rows: Vec<(String, String, String, Option<String>)>) -> Vec<DbObject> {
    rows.into_iter()
        .filter_map(|(owner, name, ty, last_ddl)| {
            Some(DbObject {
                owner,
                name,
                object_type: ObjectType::from_dictionary(&ty)?,
                valid: true,
                last_ddl: last_ddl.unwrap_or_default(),
            })
        })
        .collect()
}

/// The objects of `object_type` in a schema (the first parameter) whose
/// uppercase name is like the second, as `(owner, name, type, last change)`.
fn object_sql(object_type: ObjectType) -> Option<String> {
    Some(match object_type {
        ObjectType::Table | ObjectType::View => format!(
            "select table_schema, table_name, \
                    if(table_type = 'VIEW', 'VIEW', 'TABLE'), \
                    date_format(coalesce(update_time, create_time), {DDL_TIME}) \
               from information_schema.tables \
              where table_schema = ? and upper(table_name) like ? \
                and table_type {} 'VIEW' \
              order by table_name",
            if object_type == ObjectType::View {
                "="
            } else {
                "<>"
            }
        ),
        ObjectType::Procedure | ObjectType::Function => format!(
            "select routine_schema, routine_name, routine_type, \
                    date_format(last_altered, {DDL_TIME}) \
               from information_schema.routines \
              where routine_schema = ? and upper(routine_name) like ? and routine_type = '{}' \
              order by routine_name",
            object_type.dictionary_name()
        ),
        ObjectType::Trigger => format!(
            "select trigger_schema, trigger_name, 'TRIGGER', date_format(created, {DDL_TIME}) \
               from information_schema.triggers \
              where trigger_schema = ? and upper(trigger_name) like ? \
              order by trigger_name"
        ),
        _ => return None,
    })
}

fn objects(
    conn: &mut Conn,
    owner: &str,
    object_type: ObjectType,
    filter: Option<&str>,
) -> Result<Vec<DbObject>, DbError> {
    let Some(sql) = object_sql(object_type) else {
        return Ok(Vec::new());
    };
    let pattern = like_pattern(filter.unwrap_or(""));
    Ok(object_rows(conn.exec(sql, (owner, pattern))?))
}

/// Tables, views, routines and triggers of every schema whose name matches;
/// the current schema's first, then the shortest names.
fn find(conn: &mut Conn, pattern: &str, limit: usize) -> Result<Vec<DbObject>, DbError> {
    let sql = format!(
        "select owner, name, type, changed from ( \
           select table_schema as owner, table_name as name, \
                  if(table_type = 'VIEW', 'VIEW', 'TABLE') as type, \
                  date_format(coalesce(update_time, create_time), {DDL_TIME}) as changed \
             from information_schema.tables where upper(table_name) like ? \
           union all \
           select routine_schema, routine_name, routine_type, date_format(last_altered, {DDL_TIME}) \
             from information_schema.routines where upper(routine_name) like ? \
           union all \
           select trigger_schema, trigger_name, 'TRIGGER', date_format(created, {DDL_TIME}) \
             from information_schema.triggers where upper(trigger_name) like ? \
         ) o \
         order by case when owner = database() then 0 else 1 end, length(name), name, owner \
         limit {limit}"
    );
    let pattern = like_pattern(pattern);
    Ok(object_rows(conn.exec(sql, (&pattern, &pattern, &pattern))?))
}

/// The name as the dictionary has it: names come from the browser as
/// stored, from code assistance in uppercase. Tables are case sensitive on
/// most platforms, so look for an exact match first.
fn stored_name(conn: &mut Conn, owner: &str, table: &str) -> Result<String, DbError> {
    let exact: Option<String> = conn.exec_first(
        "select table_name from information_schema.tables \
          where table_schema = ? and table_name = ?",
        (owner, table),
    )?;
    if let Some(name) = exact {
        return Ok(name);
    }
    let folded: Option<String> = conn.exec_first(
        "select table_name from information_schema.tables \
          where table_schema = ? and lower(table_name) = lower(?) order by table_name limit 1",
        (owner, table),
    )?;
    Ok(folded.unwrap_or_else(|| table.to_owned()))
}

fn columns(
    conn: &mut Conn,
    owner: &str,
    table: &str,
    keys: bool,
) -> Result<Vec<TableColumn>, DbError> {
    let table = stored_name(conn, owner, table)?;
    let pk = if keys {
        "(select k.ordinal_position from information_schema.key_column_usage k \
           where k.table_schema = c.table_schema and k.table_name = c.table_name \
             and k.column_name = c.column_name and k.constraint_name = 'PRIMARY')"
    } else {
        "null"
    };
    let sql = format!(
        "select c.column_name, c.column_type, c.is_nullable, c.column_default, \
                nullif(c.column_comment, ''), {pk} \
           from information_schema.columns c \
          where c.table_schema = ? and c.table_name = ? \
          order by c.ordinal_position"
    );
    type Row = (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<u32>,
    );
    let rows: Vec<Row> = conn.exec(sql, (owner, &table))?;
    Ok(rows
        .into_iter()
        .map(
            |(name, data_type, nullable, default, comment, pk)| TableColumn {
                name,
                data_type,
                nullable: nullable == "YES",
                default,
                comment,
                pk_position: pk,
            },
        )
        .collect())
}

/// A key part: its column, or a functional one's expression (MySQL
/// 8.0.13) in parentheses as the DDL writes it (#258): it has no column.
/// information_schema escapes the expression's strings once more
/// (`_utf8mb4\'x\'`); `char(92)` is a backslash whatever
/// `NO_BACKSLASH_ESCAPES` says, `char(39)` a quote.
const KEY_PART: &str = "coalesce(column_name, concat('(', \
     replace(replace(expression, repeat(char(92 using utf8mb4), 2), char(92 using utf8mb4)), \
             concat(char(92 using utf8mb4), char(39 using utf8mb4)), char(39 using utf8mb4)), \
     ')'))";

/// A table's indexes, with their key parts as `part` gives them.
fn indexes_sql(part: &str) -> String {
    format!(
        "select index_name as INDEX_NAME, index_type as INDEX_TYPE, \
                if(non_unique = 0, 'UNIQUE', 'NONUNIQUE') as UNIQUENESS, \
                group_concat({part} order by seq_in_index separator ', ') as COLUMNS \
           from information_schema.statistics \
          where table_schema = ? and table_name = ? \
          group by index_name, index_type, non_unique \
          order by index_name = 'PRIMARY' desc, index_name"
    )
}

fn details(
    conn: &mut Conn,
    owner: &str,
    name: &str,
    detail: Detail,
) -> Result<MetaResult, DbError> {
    let indexes;
    let sql = match detail {
        Detail::Indexes => {
            indexes = indexes_sql(KEY_PART);
            &indexes
        }
        Detail::Constraints => {
            "select t.constraint_name as CONSTRAINT_NAME, \
                    case t.constraint_type when 'PRIMARY KEY' then 'Primary key' \
                         when 'UNIQUE' then 'Unique' when 'FOREIGN KEY' then 'Foreign key' \
                         when 'CHECK' then 'Check' else t.constraint_type end as TYPE, \
                    (select group_concat(k.column_name order by k.ordinal_position separator ', ') \
                       from information_schema.key_column_usage k \
                      where k.constraint_schema = t.constraint_schema \
                        and k.constraint_name = t.constraint_name \
                        and k.table_name = t.table_name) as COLUMNS, \
                    r.referenced_table_name as `REFERENCES`, \
                    r.delete_rule as DELETE_RULE, r.update_rule as UPDATE_RULE \
               from information_schema.table_constraints t \
               left join information_schema.referential_constraints r \
                 on r.constraint_schema = t.constraint_schema \
                and r.constraint_name = t.constraint_name and r.table_name = t.table_name \
              where t.table_schema = ? and t.table_name = ? \
              order by field(t.constraint_type, 'PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY'), \
                       t.constraint_name"
        }
        Detail::Triggers => {
            "select trigger_name as TRIGGER_NAME, \
                    concat(action_timing, ' ', event_manipulation) as TRIGGERING_EVENT, \
                    action_orientation as FOR_EACH, action_order as `ORDER` \
               from information_schema.triggers \
              where event_object_schema = ? and event_object_table = ? \
              order by action_timing, event_manipulation, action_order"
        }
        Detail::Grants => {
            "select grantee as GRANTEE, privilege_type as PRIVILEGE, is_grantable as GRANTABLE \
               from information_schema.table_privileges \
              where table_schema = ? and table_name = ? \
              order by grantee, privilege_type"
        }
        Detail::Uses => {
            "select table_schema as OWNER, table_name as NAME, 'TABLE' as TYPE \
               from information_schema.view_table_usage \
              where view_schema = ? and view_name = ? \
              order by 1, 2"
        }
        Detail::UsedBy => {
            "select view_schema as OWNER, view_name as NAME, 'VIEW' as TYPE \
               from information_schema.view_table_usage \
              where table_schema = ? and table_name = ? \
             union all \
             select constraint_schema, table_name, 'TABLE' \
               from information_schema.referential_constraints \
              where unique_constraint_schema = ? and referenced_table_name = ? \
              order by 1, 3, 2"
        }
        Detail::Link => return Err(DbError::unsupported("Database links", Dialect::MYSQL)),
    };
    let params = || -> Params {
        if detail == Detail::UsedBy {
            (owner, name, owner, name).into()
        } else {
            (owner, name).into()
        }
    };
    match table(conn, sql, params(), &["ORDER"]) {
        // No view_table_usage (MariaDB, MySQL before 8.0.13): the view
        // definitions then.
        Err(e) if e.code == Some(1109) && matches!(detail, Detail::Uses | Detail::UsedBy) => {
            table(conn, &view_usage_by_definition(detail), params(), &[])
        }
        // No `expression` (MariaDB, MySQL before 8.0.13), so no functional
        // key parts: the columns then.
        Err(e) if e.code == Some(1054) && detail == Detail::Indexes => {
            table(conn, &indexes_sql("column_name"), params(), &[])
        }
        result => result,
    }
}

/// Uses / Used by from `information_schema.views`: the server writes each
/// table in a view's definition as `` `schema`.`table` ``.
fn view_usage_by_definition(detail: Detail) -> String {
    // A LIKE pattern for `schema`.`table` anywhere, from SQL expressions.
    let mentions = |schema: &str, table: &str| {
        let quoted = |e: &str| {
            format!(
                "replace(replace(replace(replace({e}, '!', '!!'), '%', '!%'), '_', '!_'), \
                 '`', '``')"
            )
        };
        format!(
            "v.view_definition like concat('%`', {}, '`.`', {}, '`%') escape '!'",
            quoted(schema),
            quoted(table)
        )
    };
    if detail == Detail::Uses {
        format!(
            "select t.table_schema as OWNER, t.table_name as NAME, 'TABLE' as TYPE \
               from information_schema.views v \
               join information_schema.tables t on {} \
              where v.table_schema = ? and v.table_name = ? \
              order by 1, 2",
            mentions("t.table_schema", "t.table_name")
        )
    } else {
        format!(
            "select v.table_schema as OWNER, v.table_name as NAME, 'VIEW' as TYPE \
               from information_schema.views v \
              where {} \
             union all \
             select constraint_schema, table_name, 'TABLE' \
               from information_schema.referential_constraints \
              where unique_constraint_schema = ? and referenced_table_name = ? \
              order by 1, 3, 2",
            mentions("?", "?")
        )
    }
}

/// Runs a query into a generic table; `numeric` names right-aligned columns.
fn table(
    conn: &mut Conn,
    sql: &str,
    params: Params,
    numeric: &[&str],
) -> Result<MetaResult, DbError> {
    let result = conn.exec_iter(sql, params)?;
    let columns = result
        .columns()
        .as_ref()
        .iter()
        .map(|c| Column {
            numeric: numeric.contains(&c.name_str().as_ref()),
            ..column(c)
        })
        .collect();
    let rows = all_rows(result)?;
    Ok(MetaResult::Table { columns, rows })
}

/// `SHOW CREATE …` of an object: its complete, runnable DDL.
fn show_create(
    conn: &mut Conn,
    owner: &str,
    name: &str,
    object_type: ObjectType,
) -> Result<String, DbError> {
    let (what, column) = match object_type {
        ObjectType::Table => ("table", 1),
        ObjectType::View => ("view", 1),
        ObjectType::Procedure => ("procedure", 2),
        ObjectType::Function => ("function", 2),
        ObjectType::Trigger => ("trigger", 2),
        other => {
            return Err(DbError::new(format!(
                "MySQL has no {}",
                other.plural().to_lowercase()
            )));
        }
    };
    let dialect = Dialect::MYSQL;
    let sql = format!("show create {what} {}", dialect.qualified(owner, name));
    let row: Option<mysql::Row> = conn.query_first(sql)?;
    let ddl = row
        .and_then(|row| row.get_opt::<Option<String>, _>(column))
        .and_then(Result::ok)
        .flatten();
    ddl.ok_or_else(|| {
        DbError::new(format!(
            "No source for {what} {owner}.{name}: it takes the SHOW_ROUTINE privilege, or being \
             its definer, to see it"
        ))
    })
}

/// The plan of `sql` as `PLAN_TABLE`-like rows (ID, PARENT_ID, DEPTH,
/// OPERATION, OBJECT, COST, ROWS) for the plan view, from MySQL's tree
/// format. Servers without it (MariaDB, MySQL before 8.0.16) give the
/// classic table, one row per table: MariaDB, which knows `FORMAT=`, says
/// 1791 (unknown format), servers that don't, 1064.
fn explain_plan(conn: &mut Conn, sql: &str) -> Result<MetaResult, DbError> {
    let tree: Result<Option<String>, _> = conn.query_first(format!("explain format=tree {sql}"));
    let rows = match tree {
        Ok(Some(tree)) => tree_rows(&tree),
        Ok(None) => Vec::new(),
        Err(mysql::Error::MySqlError(e)) if matches!(e.code, 1064 | 1791) => {
            classic_rows(conn, sql)?
        }
        Err(e) => return Err(e.into()),
    };
    let column = |name: &str, numeric: bool| Column {
        name: name.into(),
        type_name: if numeric { "DOUBLE" } else { "VARCHAR" }.into(),
        numeric,
    };
    Ok(MetaResult::Table {
        columns: vec![
            column("ID", true),
            column("PARENT_ID", true),
            column("DEPTH", true),
            column("OPERATION", false),
            column("OBJECT", false),
            column("COST", true),
            column("ROWS", true),
            column("FILTER_PRED", false),
        ],
        rows,
    })
}

/// Parses `EXPLAIN FORMAT=TREE` output: one `-> operation  (cost=… rows=…)`
/// line per step, indented four spaces per level.
fn tree_rows(tree: &str) -> Vec<Vec<Cell>> {
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    // Ids of the open ancestors, by depth.
    let mut parents: Vec<usize> = Vec::new();
    for line in tree.lines() {
        let trimmed = line.trim_start();
        let Some(step) = trimmed.strip_prefix("-> ") else {
            // A wrapped operation continues on the next line.
            if let Some(Some(op)) = rows.last_mut().map(|r| &mut r[3]) {
                op.push(' ');
                op.push_str(trimmed);
            }
            continue;
        };
        let depth = (line.len() - trimmed.len()) / 4;
        let id = rows.len() + 1;
        parents.truncate(depth);
        let parent = parents.last().copied();
        parents.push(id);
        let (operation, cost, estimated) = split_estimates(step);
        let (operation, filter) = match operation.strip_prefix("Filter: ") {
            Some(condition) => ("Filter".to_owned(), Some(condition.to_owned())),
            None => (operation.to_owned(), None),
        };
        let object = operation
            .split_once(" on ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .map(str::to_owned);
        rows.push(vec![
            Some(id.to_string()),
            parent.map(|p| p.to_string()),
            Some(depth.to_string()),
            Some(operation),
            object,
            cost,
            estimated,
            filter,
        ]);
    }
    rows
}

/// `Table scan on e  (cost=0.45 rows=2)` → the operation, cost and rows.
fn split_estimates(step: &str) -> (&str, Option<String>, Option<String>) {
    let Some(start) = step.rfind("  (") else {
        return (step.trim_end(), None, None);
    };
    let estimates = step[start + 3..].trim_end_matches(')');
    let value = |key: &str| {
        estimates
            .split_whitespace()
            .find_map(|part| part.strip_prefix(key))
            .map(str::to_owned)
    };
    (step[..start].trim_end(), value("cost="), value("rows="))
}

fn classic_rows(conn: &mut Conn, sql: &str) -> Result<Vec<Vec<Cell>>, DbError> {
    let rows: Vec<mysql::Row> = conn.query(format!("explain {sql}"))?;
    let text = |row: &mysql::Row, name: &str| -> Cell {
        let i = row
            .columns_ref()
            .iter()
            .position(|c| c.name_str() == name)?;
        match row.as_ref(i)? {
            Value::NULL => None,
            Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            other => Some(other.as_sql(true).trim_matches('\'').to_owned()),
        }
    };
    Ok(rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let operation = [text(row, "select_type"), text(row, "type")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            vec![
                Some((i + 1).to_string()),
                None,
                Some("0".into()),
                Some(operation),
                text(row, "table"),
                None,
                text(row, "rows"),
                text(row, "Extra"),
            ]
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_tree() {
        let tree = "-> Nested loop inner join  (cost=1.05 rows=2)\n    \
                    -> Filter: (e.deptno is not null)  (cost=0.45 rows=2)\n        \
                    -> Table scan on e  (cost=0.45 rows=2)\n    \
                    -> Single-row index lookup on d using PRIMARY (deptno=e.deptno)  (cost=0.28 rows=1)\n";
        let rows = tree_rows(tree);
        let get = |r: usize, c: usize| rows[r][c].clone().unwrap_or_default();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            (get(0, 0), get(0, 1), get(0, 3), get(0, 5), get(0, 6)),
            (
                "1".into(),
                "".into(),
                "Nested loop inner join".into(),
                "1.05".into(),
                "2".into()
            )
        );
        assert_eq!(
            (get(1, 1), get(1, 3), get(1, 7)),
            ("1".into(), "Filter".into(), "(e.deptno is not null)".into())
        );
        assert_eq!(
            (get(2, 1), get(2, 2), get(2, 4)),
            ("2".into(), "2".into(), "e".into())
        );
        assert_eq!((get(3, 1), get(3, 4)), ("1".into(), "d".into()));
    }
}
