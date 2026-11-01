//! SQLite's answers to [`MetaQuery`]s, from each database's `sqlite_schema`
//! and the table-valued pragmas (`pragma_table_xinfo`, `pragma_index_list`,
//! `pragma_foreign_key_list`). Schemas are the attached databases (`main`,
//! `temp` when it has objects, those of `ATTACH`). Names keep their case and
//! are compared without it, as SQLite does.

use pythia_plsql::{Syntax, TokenKind, tokenize};
use rusqlite::Connection;

use crate::dialect::Dialect;
use crate::meta::{DbObject, Detail, MetaQuery, MetaResult, ObjectType, TableColumn};
use crate::sqlite_session::all_rows;
use crate::types::{Cell, Column, DbError};

pub(crate) fn run(conn: &Connection, query: &MetaQuery) -> Result<MetaResult, DbError> {
    Ok(match query {
        MetaQuery::Schemas => MetaResult::Schemas(schemas(conn)?),
        MetaQuery::Objects {
            owner,
            object_type,
            filter,
        } => MetaResult::Objects(objects(conn, owner, *object_type, filter.as_deref())?),
        MetaQuery::Columns { owner, table, .. } => {
            MetaResult::Columns(columns(conn, owner, table)?)
        }
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
        } => MetaResult::Text(source(conn, owner, name, *object_type)?),
        MetaQuery::Ddl {
            owner,
            name,
            object_type,
        } => MetaResult::Text(ddl(conn, owner, name, *object_type)?),
        // SQLite checks a statement when it is created.
        MetaQuery::Errors { .. } => MetaResult::Errors(Vec::new()),
        MetaQuery::Find { pattern, limit } => MetaResult::Objects(find(conn, pattern, *limit)?),
        MetaQuery::ExplainPlan { sql } => explain_plan(conn, sql)?,
        MetaQuery::TestLink { .. } => {
            return Err(DbError::unsupported("Database links", Dialect::SQLITE));
        }
        // A file has no users.
        MetaQuery::Accounts(_) => MetaResult::Accounts(Vec::new()),
    })
}

/// `"schema".sqlite_schema`, quoted as it must be.
fn schema_table(schema: &str) -> String {
    format!("{}.sqlite_schema", quote(schema))
}

fn quote(name: &str) -> String {
    Dialect::SQLITE.quote_ident(name)
}

/// The attached databases: `main`, the others in the order they were
/// attached, `temp` only once something was created in it.
fn schemas(conn: &Connection) -> Result<Vec<String>, DbError> {
    let mut statement = conn.prepare("select name from pragma_database_list order by seq")?;
    let mut names = Vec::new();
    for row in all_rows(&mut statement, &[])? {
        let Some(Some(name)) = row.into_iter().next() else {
            continue;
        };
        if name == "temp" {
            let used: i64 =
                conn.query_row("select count(*) from temp.sqlite_schema", [], |r| r.get(0))?;
            if used == 0 {
                continue;
            }
        }
        names.push(name);
    }
    Ok(names)
}

/// The type of `object_type` in `sqlite_schema`.
fn schema_type(object_type: ObjectType) -> Option<&'static str> {
    Some(match object_type {
        ObjectType::Table => "table",
        ObjectType::View => "view",
        ObjectType::Index => "index",
        ObjectType::Trigger => "trigger",
        _ => return None,
    })
}

/// SQLite's own tables (`sqlite_sequence`, `sqlite_stat1`) and the indexes
/// it makes for constraints (`sqlite_autoindex_…`) aren't listed.
const NOT_INTERNAL: &str = "name not like 'sqlite!_%' escape '!'";

fn objects(
    conn: &Connection,
    owner: &str,
    object_type: ObjectType,
    filter: Option<&str>,
) -> Result<Vec<DbObject>, DbError> {
    let Some(kind) = schema_type(object_type) else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "select name from {} where type = ? and {NOT_INTERNAL} order by name",
        schema_table(owner)
    );
    let filter = filter.unwrap_or("");
    let mut statement = conn.prepare(&sql)?;
    Ok(all_rows(&mut statement, &[kind])?
        .into_iter()
        .filter_map(|row| row.into_iter().next()?)
        .filter(|name| filter_matches(filter, name))
        .map(|name| DbObject {
            owner: owner.to_owned(),
            name,
            object_type,
            valid: true,
            // SQLite doesn't keep when an object changed.
            last_ddl: String::new(),
        })
        .collect())
}

/// Whether a name contains the filter, as in
/// [`like_pattern`](crate::meta::like_pattern): `*` is any text, `?` any
/// character, and each letter stands for its cases as Rust has them.
/// SQLite's `upper()` and `LIKE` change ASCII letters only, `upper('čaj')`
/// is `čAJ`, so the names are matched here.
fn filter_matches(filter: &str, name: &str) -> bool {
    // A letter's uppercase where that is one letter: ß stays ß.
    let fold = |c: char| {
        let mut upper = c.to_uppercase();
        match (upper.next(), upper.next()) {
            (Some(one), None) => one,
            _ => c,
        }
    };
    // Contains: `*pattern`, at the start of a name that goes on.
    let pattern: Vec<char> = std::iter::once('*')
        .chain(filter.trim().chars().map(fold))
        .collect();
    let name: Vec<char> = name.chars().map(fold).collect();
    glob(&pattern, &name)
}

/// Whether `pattern` matches the start of `name`, `*` and `?` as wildcards.
fn glob(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    // The last `*` and the place in the name it took up to.
    let mut star: Option<(usize, usize)> = None;
    while p < pattern.len() {
        match pattern[p] {
            '*' => {
                star = Some((p, n));
                p += 1;
            }
            c if n < name.len() && (c == '?' || c == name[n]) => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((at, taken)) if taken < name.len() => {
                    star = Some((at, taken + 1));
                    p = at + 1;
                    n = taken + 1;
                }
                _ => return false,
            },
        }
    }
    true
}

/// Tables, views, indexes and triggers of every database whose name
/// matches: `main`'s first, then the shortest names.
fn find(conn: &Connection, pattern: &str, limit: usize) -> Result<Vec<DbObject>, DbError> {
    let mut found = Vec::new();
    for (order, schema) in schemas(conn)?.into_iter().enumerate() {
        let sql = format!(
            "select name, type from {} \
              where type in ('table', 'view', 'index', 'trigger') and {NOT_INTERNAL}",
            schema_table(&schema)
        );
        let mut statement = conn.prepare(&sql)?;
        for row in all_rows(&mut statement, &[])? {
            let mut values = row.into_iter();
            let (Some(Some(name)), Some(Some(kind))) = (values.next(), values.next()) else {
                continue;
            };
            if !filter_matches(pattern, &name) {
                continue;
            }
            let Some(object_type) = ObjectType::from_dictionary(&kind.to_uppercase()) else {
                continue;
            };
            found.push((
                order,
                DbObject {
                    owner: schema.clone(),
                    name,
                    object_type,
                    valid: true,
                    last_ddl: String::new(),
                },
            ));
        }
    }
    found.sort_by(|(a_order, a), (b_order, b)| {
        (a_order, a.name.len(), &a.name).cmp(&(b_order, b.name.len(), &b.name))
    });
    Ok(found.into_iter().take(limit).map(|(_, o)| o).collect())
}

/// The columns of a table or view; generated columns too, a virtual
/// table's hidden ones not.
fn columns(conn: &Connection, owner: &str, table: &str) -> Result<Vec<TableColumn>, DbError> {
    let mut statement = conn.prepare(
        "select name, type, \"notnull\", dflt_value, pk from pragma_table_xinfo(?, ?) \
          where hidden <> 1 order by cid",
    )?;
    Ok(all_rows(&mut statement, &[table, owner])?
        .into_iter()
        .map(|row| {
            let value = |i: usize| row.get(i).cloned().flatten();
            let pk = value(4)
                .and_then(|p| p.parse::<u32>().ok())
                .filter(|p| *p > 0);
            TableColumn {
                name: value(0).unwrap_or_default(),
                data_type: value(1).unwrap_or_default(),
                // A rowid alias (INTEGER PRIMARY KEY) can't be NULL either.
                nullable: value(2).as_deref() != Some("1")
                    && !(pk.is_some() && is_rowid_alias(conn, owner, table)),
                default: value(3),
                comment: None,
                pk_position: pk,
            }
        })
        .collect())
}

/// Whether the table's primary key is its rowid: a single INTEGER column,
/// in a table with rowids.
fn is_rowid_alias(conn: &Connection, owner: &str, table: &str) -> bool {
    let sql = format!(
        "select (select count(*) from pragma_table_info(?1, ?2) where pk > 0) = 1 \
            and (select upper(type) from pragma_table_info(?1, ?2) where pk = 1) = 'INTEGER' \
            and not exists (select 1 from {} where type = 'table' and name = ?1 collate nocase \
                             and upper(sql) like '%WITHOUT%ROWID%')",
        schema_table(owner)
    );
    conn.query_row(&sql, [table, owner], |row| row.get::<_, bool>(0))
        .unwrap_or(false)
}

fn details(
    conn: &Connection,
    owner: &str,
    name: &str,
    detail: Detail,
) -> Result<MetaResult, DbError> {
    match detail {
        Detail::Indexes => table(
            conn,
            "select il.name as INDEX_NAME, \
                    case il.origin when 'pk' then 'Primary key' when 'u' then 'Unique constraint' \
                         else 'CREATE INDEX' end as ORIGIN, \
                    case il.\"unique\" when 1 then 'UNIQUE' else 'NONUNIQUE' end as UNIQUENESS, \
                    (select group_concat(coalesce(ii.name, '(expression)'), ', ' order by ii.seqno) \
                       from pragma_index_info(il.name, ?2) ii) as COLUMNS, \
                    case il.partial when 1 then 'YES' else 'NO' end as PARTIAL \
               from pragma_index_list(?1, ?2) il \
              order by il.origin = 'pk' desc, il.name",
            &[name, owner],
        ),
        Detail::Constraints => constraints(conn, owner, name),
        Detail::Triggers => {
            let sql = format!(
                "select name, sql from {} where type = 'trigger' and tbl_name = ? collate nocase \
                 order by name",
                schema_table(owner)
            );
            let mut statement = conn.prepare(&sql)?;
            let rows = all_rows(&mut statement, &[name])?
                .into_iter()
                .map(|row| {
                    let sql = row.get(1).cloned().flatten().unwrap_or_default();
                    vec![row.into_iter().next().flatten(), Some(trigger_event(&sql))]
                })
                .collect();
            Ok(MetaResult::Table {
                columns: text_columns(&["TRIGGER_NAME", "TRIGGERING_EVENT"]),
                rows,
            })
        }
        Detail::Uses => {
            // The tables and views a view's query names.
            let sql = format!(
                "select sql from {} where type = 'view' and name = ? collate nocase",
                schema_table(owner)
            );
            let text: Option<String> = conn
                .query_row(&sql, [name], |row| row.get(0))
                .unwrap_or_default();
            let named = words(text.as_deref().unwrap_or_default());
            let rows = schema_objects(conn, owner)?
                .into_iter()
                .filter(|(object, kind)| {
                    !object.eq_ignore_ascii_case(name)
                        && matches!(kind.as_str(), "table" | "view")
                        && named.iter().any(|w| w.eq_ignore_ascii_case(object))
                })
                .map(|(object, kind)| {
                    vec![
                        Some(owner.to_owned()),
                        Some(object),
                        Some(kind.to_uppercase()),
                    ]
                })
                .collect();
            Ok(MetaResult::Table {
                columns: text_columns(&["OWNER", "NAME", "TYPE"]),
                rows,
            })
        }
        Detail::UsedBy => used_by(conn, owner, name),
        Detail::Grants => Ok(MetaResult::Table {
            columns: text_columns(&["GRANTEE", "PRIVILEGE"]),
            rows: Vec::new(),
        }),
        Detail::Link => Err(DbError::unsupported("Database links", Dialect::SQLITE)),
    }
}

/// Views that name the table, and tables whose foreign keys reference it.
fn used_by(conn: &Connection, owner: &str, name: &str) -> Result<MetaResult, DbError> {
    let mut rows = Vec::new();
    let objects = schema_objects(conn, owner)?;
    let sql = format!(
        "select sql from {} where type = 'view' and name = ?",
        schema_table(owner)
    );
    for (view, _) in objects.iter().filter(|(_, kind)| kind == "view") {
        let text: Option<String> = conn
            .query_row(&sql, [view], |row| row.get(0))
            .unwrap_or_default();
        if words(text.as_deref().unwrap_or_default())
            .iter()
            .any(|w| w.eq_ignore_ascii_case(name))
        {
            rows.push(vec![
                Some(owner.to_owned()),
                Some(view.clone()),
                Some("VIEW".into()),
            ]);
        }
    }
    let sql = format!(
        "select distinct m.name from {} m, pragma_foreign_key_list(m.name, ?1) f \
          where m.type = 'table' and f.\"table\" = ?2 collate nocase and m.{NOT_INTERNAL} \
          order by m.name",
        schema_table(owner)
    );
    let mut statement = conn.prepare(&sql)?;
    for row in all_rows(&mut statement, &[owner, name])? {
        rows.push(vec![
            Some(owner.to_owned()),
            row.into_iter().next().flatten(),
            Some("TABLE".into()),
        ]);
    }
    Ok(MetaResult::Table {
        columns: text_columns(&["OWNER", "NAME", "TYPE"]),
        rows,
    })
}

/// The primary key, unique constraints and foreign keys of a table. SQLite
/// doesn't name them in its pragmas, nor list checks.
fn constraints(conn: &Connection, owner: &str, name: &str) -> Result<MetaResult, DbError> {
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut statement = conn.prepare(
        "select group_concat(name, ', ' order by pk) from pragma_table_info(?1, ?2) where pk > 0",
    )?;
    if let Some(Some(columns)) = all_rows(&mut statement, &[name, owner])?
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
    {
        rows.push(vec![
            None,
            Some("Primary key".into()),
            Some(columns),
            None,
            None,
            None,
        ]);
    }
    let mut statement = conn.prepare(
        "select il.name, (select group_concat(ii.name, ', ' order by ii.seqno) \
                            from pragma_index_info(il.name, ?2) ii) \
           from pragma_index_list(?1, ?2) il where il.origin = 'u' order by il.name",
    )?;
    for row in all_rows(&mut statement, &[name, owner])? {
        let mut values = row.into_iter();
        let index = values.next().flatten();
        rows.push(vec![
            // An autoindex's name says nothing.
            index.filter(|i| !i.starts_with("sqlite_autoindex_")),
            Some("Unique".into()),
            values.next().flatten(),
            None,
            None,
            None,
        ]);
    }
    let mut statement = conn.prepare(
        "select group_concat(\"from\", ', ' order by seq), \"table\", \
                group_concat(coalesce(\"to\", '(primary key)'), ', ' order by seq), \
                on_delete, on_update \
           from pragma_foreign_key_list(?1, ?2) group by id order by id",
    )?;
    for row in all_rows(&mut statement, &[name, owner])? {
        let value = |i: usize| row.get(i).cloned().flatten();
        let references = match (value(1), value(2)) {
            (Some(table), Some(columns)) => Some(format!("{table} ({columns})")),
            (table, _) => table,
        };
        rows.push(vec![
            None,
            Some("Foreign key".into()),
            value(0),
            references,
            value(3),
            value(4),
        ]);
    }
    Ok(MetaResult::Table {
        columns: text_columns(&[
            "CONSTRAINT_NAME",
            "TYPE",
            "COLUMNS",
            "REFERENCES",
            "DELETE_RULE",
            "UPDATE_RULE",
        ]),
        rows,
    })
}

/// `BEFORE INSERT`, `AFTER UPDATE OF name`, `INSTEAD OF DELETE`: the words
/// between a trigger's name and `ON` in its `CREATE TRIGGER`.
fn trigger_event(sql: &str) -> String {
    let words = words(sql);
    let start = words
        .iter()
        .position(|w| {
            ["BEFORE", "AFTER", "INSTEAD", "INSERT", "UPDATE", "DELETE"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
        })
        .unwrap_or(words.len());
    let end = words[start..]
        .iter()
        .position(|w| w.eq_ignore_ascii_case("ON"))
        .map_or(words.len(), |n| start + n);
    words[start..end]
        .iter()
        .map(|w| {
            if event_word(w) {
                w.to_uppercase()
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn event_word(word: &str) -> bool {
    [
        "BEFORE", "AFTER", "INSTEAD", "OF", "INSERT", "UPDATE", "DELETE",
    ]
    .iter()
    .any(|k| word.eq_ignore_ascii_case(k))
}

/// The words and names of a statement (quotes taken off), not those in its
/// strings or comments.
fn words(sql: &str) -> Vec<String> {
    tokenize(sql)
        .into_iter()
        .filter_map(|t| {
            let text = &sql[t.span];
            match t.kind {
                TokenKind::QuotedIdent => {
                    let inner = &text[1..text.len().saturating_sub(1).max(1)];
                    Some(match text.chars().next() {
                        Some('`') => inner.replace("``", "`"),
                        Some('[') => inner.to_owned(),
                        _ => inner.replace("\"\"", "\""),
                    })
                }
                TokenKind::Ident | TokenKind::Keyword => Some(text.to_owned()),
                _ => None,
            }
        })
        .collect()
}

/// `(name, type)` of the schema's objects.
fn schema_objects(conn: &Connection, owner: &str) -> Result<Vec<(String, String)>, DbError> {
    let sql = format!(
        "select name, type from {} where {NOT_INTERNAL} order by name",
        schema_table(owner)
    );
    let mut statement = conn.prepare(&sql)?;
    Ok(all_rows(&mut statement, &[])?
        .into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter();
            Some((values.next()??, values.next()??))
        })
        .collect())
}

/// An object's `CREATE` statement as it was written.
fn source(
    conn: &Connection,
    owner: &str,
    name: &str,
    object_type: ObjectType,
) -> Result<String, DbError> {
    let Some(kind) = schema_type(object_type) else {
        return Err(DbError::new(format!(
            "SQLite has no {}",
            object_type.plural().to_lowercase()
        )));
    };
    let sql = format!(
        "select sql from {} where type = ? and name = ? collate nocase",
        schema_table(owner)
    );
    let text: Option<Option<String>> = match conn.query_row(&sql, [kind, name], |row| row.get(0)) {
        Ok(text) => Some(text),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };
    match text {
        Some(Some(text)) => Ok(text),
        // An index SQLite made for a constraint has no statement.
        Some(None) => Err(DbError::new(format!(
            "{owner}.{name} was made by SQLite for a constraint of its table"
        ))),
        None => Err(DbError::new(format!("There is no {kind} {owner}.{name}"))),
    }
}

/// The DDL to create an object again: a table with its indexes.
fn ddl(
    conn: &Connection,
    owner: &str,
    name: &str,
    object_type: ObjectType,
) -> Result<String, DbError> {
    let mut out = statement(&source(conn, owner, name, object_type)?);
    if object_type == ObjectType::Table {
        let sql = format!(
            "select sql from {} where type = 'index' and tbl_name = ? collate nocase \
               and sql is not null order by name",
            schema_table(owner)
        );
        let mut indexes = conn.prepare(&sql)?;
        for row in all_rows(&mut indexes, &[name])? {
            if let Some(Some(index)) = row.into_iter().next() {
                out.push('\n');
                out.push_str(&statement(&index));
            }
        }
    }
    Ok(out)
}

/// A stored statement with its `;`: SQLite keeps the text as written, a
/// line comment at its end too (#206).
fn statement(sql: &str) -> String {
    format!("{}\n", pythia_plsql::terminated(sql, Syntax::Sqlite))
}

/// A generic table of text columns.
fn table(conn: &Connection, sql: &str, params: &[&str]) -> Result<MetaResult, DbError> {
    let mut statement = conn.prepare(sql)?;
    let columns = statement
        .column_names()
        .map(|name| Column {
            name: name.to_owned(),
            type_name: "TEXT".into(),
            numeric: false,
        })
        .collect();
    let rows = all_rows(&mut statement, params)?;
    Ok(MetaResult::Table { columns, rows })
}

fn text_columns(names: &[&str]) -> Vec<Column> {
    names
        .iter()
        .map(|name| Column {
            name: (*name).to_owned(),
            type_name: "TEXT".into(),
            numeric: false,
        })
        .collect()
}

/// The plan of `sql` as `PLAN_TABLE`-like rows (ID, PARENT_ID, DEPTH,
/// OPERATION, OBJECT) for the plan view, from `EXPLAIN QUERY PLAN`: one row
/// per step, with the step it belongs to.
fn explain_plan(conn: &Connection, sql: &str) -> Result<MetaResult, DbError> {
    let sql = sql.trim().trim_end_matches(';');
    let mut statement = conn.prepare(&format!("explain query plan {sql}"))?;
    let mut steps: Vec<(i64, i64, String)> = Vec::new();
    // Bind variables stay unbound (NULL): the plan doesn't depend on them.
    let mut rows = statement.raw_query();
    while let Some(row) = rows.next()? {
        steps.push((row.get(0)?, row.get(1)?, row.get(3)?));
    }
    let column = |name: &str, numeric: bool| Column {
        name: name.into(),
        type_name: if numeric { "INTEGER" } else { "TEXT" }.into(),
        numeric,
    };
    Ok(MetaResult::Table {
        columns: vec![
            column("ID", true),
            column("PARENT_ID", true),
            column("DEPTH", true),
            column("OPERATION", false),
            column("OBJECT", false),
        ],
        rows: plan_rows(&steps),
    })
}

/// `(id, parent, detail)` steps → plan rows numbered from 1 in their order;
/// SQLite's ids are its own, the root steps' parent 0.
fn plan_rows(steps: &[(i64, i64, String)]) -> Vec<Vec<Cell>> {
    let position = |id: i64| steps.iter().position(|(i, _, _)| *i == id);
    steps
        .iter()
        .map(|(_, parent, detail)| {
            let parent = position(*parent);
            let mut depth = 0;
            let mut up = parent;
            while let Some(p) = up {
                depth += 1;
                up = position(steps[p].1).filter(|_| depth < steps.len());
            }
            vec![
                None,
                parent.map(|p| (p + 1).to_string()),
                Some(depth.to_string()),
                Some(detail.clone()),
                plan_object(detail),
            ]
        })
        .enumerate()
        .map(|(i, mut row)| {
            row[0] = Some((i + 1).to_string());
            row
        })
        .collect()
}

/// The table of `SCAN t`, `SEARCH t USING …`.
fn plan_object(detail: &str) -> Cell {
    let mut words = detail.split_whitespace();
    match words.next()? {
        "SCAN" | "SEARCH" => words
            .next()
            .filter(|w| !["TABLE", "SUBQUERY", "CONSTANT"].contains(w))
            .map(str::to_owned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match() {
        for (filter, name) in [
            ("", "t"),
            ("čaj", "r62_čaj"),
            ("ČAJ", "r62_čaj"),
            (" bière ", "BIÈRE"),
            ("öl", "ÖL"),
            ("σ", "Σίσυφος"),
            ("ß", "Straße"),
            ("a*b", "xaxxbx"),
            ("a?c", "abc"),
            ("*", ""),
            ("100%", "r_100%"),
            ("a_b", "A_B"),
        ] {
            assert!(filter_matches(filter, name), "{filter} {name}");
        }
        for (filter, name) in [
            ("čaj", "caj"),
            ("ss", "Straße"),
            ("a?c", "ac"),
            ("a*b", "ba"),
            ("100%", "1000"),
            ("a_b", "axb"),
            ("x", ""),
        ] {
            assert!(!filter_matches(filter, name), "{filter} {name}");
        }
    }

    #[test]
    fn trigger_events() {
        assert_eq!(
            trigger_event("CREATE TRIGGER t_audit AFTER update of name ON t BEGIN select 1; END"),
            "AFTER UPDATE OF name"
        );
        assert_eq!(
            trigger_event("create temp trigger if not exists x instead of delete on v begin end"),
            "INSTEAD OF DELETE"
        );
        assert_eq!(
            trigger_event("create trigger \"Ins\" insert on t begin end"),
            "INSERT"
        );
    }

    #[test]
    fn plan() {
        let steps = vec![
            (2, 0, "SCAN e".to_owned()),
            (
                4,
                0,
                "SEARCH d USING INTEGER PRIMARY KEY (rowid=?)".to_owned(),
            ),
            (7, 0, "CORRELATED SCALAR SUBQUERY 1".to_owned()),
            (11, 7, "SCAN x".to_owned()),
            (18, 0, "USE TEMP B-TREE FOR ORDER BY".to_owned()),
        ];
        let rows = plan_rows(&steps);
        let get = |r: usize, c: usize| rows[r][c].clone().unwrap_or_default();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            (get(0, 0), get(0, 1), get(0, 2), get(0, 4)),
            ("1".into(), "".into(), "0".into(), "e".into())
        );
        assert_eq!(get(1, 4), "d");
        assert_eq!(
            (get(3, 1), get(3, 2), get(3, 4)),
            ("3".into(), "1".into(), "x".into())
        );
        assert_eq!(get(4, 4), "");
    }

    #[test]
    fn view_words() {
        assert_eq!(
            words("select a.x from \"My Table\" a join [b] on 1 -- emp\n where y = 'dept'"),
            [
                "select", "a", "x", "from", "My Table", "a", "join", "b", "on", "where", "y"
            ]
        );
    }
}
