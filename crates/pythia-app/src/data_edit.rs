//! Editing query results: finding the base table and turning grid changes
//! into DML.
//!
//! On Oracle, rows are identified by ROWID, fetched by wrapping the user's
//! query: `select q.*, q.rowid as pythia$rowid from (<query>) q` — Oracle
//! itself rejects shapes that aren't key-preserved (ORA-01446). MySQL,
//! PostgreSQL and SQL Server have no ROWID that lasts (PostgreSQL's `ctid`
//! moves with every update): there the table's primary key (or a unique key
//! on NOT NULL columns) identifies a row, its columns added to the select
//! list as `pythia$<column>`. Nothing is locked while editing; instead every
//! UPDATE/DELETE re-checks the values that were fetched (optimistic
//! concurrency), so a row changed by another session is reported as a
//! conflict rather than silently overwritten.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pythia_data::literal;
use pythia_db::{Bind, Cell, Client, Column, DbError, DbKind, Dialect};
use pythia_plsql::{TokenKind, tokenize, tokenize_in};
use pythia_ui::{Changes, Loaded, RowRef};

/// Name of the ROWID column added to editable queries.
pub const ROWID_COLUMN: &str = "PYTHIA$ROWID";

/// Functions that fold rows into one (unless used with `OVER`).
const AGGREGATES: &[&str] = &[
    "AVG",
    "BIT_AND",
    "BIT_OR",
    "BIT_XOR",
    "COUNT",
    "GROUP_CONCAT",
    "JSON_ARRAYAGG",
    "JSON_OBJECTAGG",
    "LISTAGG",
    "MAX",
    "MEDIAN",
    "MIN",
    "STD",
    "STDDEV",
    "STDDEV_POP",
    "STDDEV_SAMP",
    "SUM",
    "VAR_POP",
    "VAR_SAMP",
    "VARIANCE",
];

/// How the rows of an editable result are found again. The result ends
/// with the columns that identify them, read-only in the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    /// Oracle: one column with the ROWID.
    Rowid,
    /// The table's key columns, in key order.
    Columns(Vec<String>),
}

impl RowKey {
    /// How many columns at the end of the result hold the key.
    pub fn width(&self) -> usize {
        match self {
            RowKey::Rowid => 1,
            RowKey::Columns(columns) => columns.len(),
        }
    }
}

/// A result being edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editable {
    /// The table changes go to, as written in the query.
    pub table: String,
    /// The query as the user wrote it.
    pub source: String,
    pub key: RowKey,
}

/// Wraps a query so the result carries the base table's ROWID.
pub fn editable_query(sql: &str) -> String {
    format!(
        "select q.*, q.rowid as {} from (\n{}\n) q",
        ROWID_COLUMN.to_lowercase(),
        sql.trim().trim_end_matches(';')
    )
}

/// Adds the key columns to a query's select list, as `pythia$<column>`
/// quoted the way of `dialect`. `None` unless [`can_edit`].
pub fn keyed_query(dialect: Dialect, sql: &str, key: &[String]) -> Option<String> {
    let sql = sql.trim().trim_end_matches(';');
    let query = parse(dialect, sql).filter(|q| !q.grouped)?;
    let columns: Vec<String> = key
        .iter()
        .map(|k| {
            format!(
                "{} as {}",
                dialect.quote_ident(k),
                dialect.quote_ident(&format!("pythia${k}"))
            )
        })
        .collect();
    Some(format!(
        "{}, {}{}",
        &sql[..query.list_end],
        columns.join(", "),
        &sql[query.list_end..]
    ))
}

/// Whether `columns`, the result of [`edit_query`], end with the key
/// columns it added: with a select list misread (a comment the lexer
/// didn't know), they may be missing, and the columns before them would
/// be taken for the key.
pub fn has_key_columns(edit: &Editable, columns: &[Column]) -> bool {
    let names: Vec<String> = match &edit.key {
        RowKey::Rowid => vec![ROWID_COLUMN.to_owned()],
        RowKey::Columns(key) => key.iter().map(|k| format!("pythia${k}")).collect(),
    };
    // PostgreSQL cuts a long name at 63 bytes.
    let is = |column: &str, name: &str| {
        let (column, name) = (column.to_lowercase(), name.to_lowercase());
        column.starts_with("pythia$") && name.starts_with(&column)
    };
    columns.len() >= names.len()
        && columns[columns.len() - names.len()..]
            .iter()
            .zip(&names)
            .all(|(column, name)| is(&column.name, name))
}

/// The query fetching the rows of `edit` with what identifies them, in
/// `dialect`'s SQL.
pub fn edit_query(dialect: Dialect, edit: &Editable) -> Option<String> {
    match &edit.key {
        RowKey::Rowid => Some(editable_query(&edit.source)),
        RowKey::Columns(key) => keyed_query(dialect, &edit.source, key),
    }
}

/// A simple single-table query, as [`parse`] finds it.
struct Query {
    /// The table, as written.
    table: String,
    /// Where the select list ends (after its last token).
    list_end: usize,
    /// DISTINCT, GROUP BY, HAVING or an aggregate: a row of the result
    /// isn't a row of the table.
    grouped: bool,
    /// The select list, item by item.
    items: Vec<Item>,
}

/// An item of a select list, as editing sees it (#218).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// `*` or `t.*`: the table's columns, as they are.
    Star,
    /// A column of the table as it is: `col`, `t.col`, `"Col"`.
    Column,
    /// Anything else: an expression, a column renamed (`x as y`).
    Other,
}

/// Words that may come before a select list's first item.
const LIST_MODIFIERS: &[&str] = &[
    "ALL",
    "DISTINCT",
    "DISTINCTROW",
    "UNIQUE",
    "HIGH_PRIORITY",
    "STRAIGHT_JOIN",
    "SQL_SMALL_RESULT",
    "SQL_BIG_RESULT",
    "SQL_BUFFER_RESULT",
    "SQL_NO_CACHE",
    "SQL_CACHE",
    "SQL_CALC_FOUND_ROWS",
];

/// `sql` with its quoted names the way the lexer reads them, each where it
/// is in `sql` so that spans still hold: SQL Server's `[name]` as
/// `"name"`, and a doubled `""` inside a name as `__` (the lexer ends a
/// name at the next `"`). A quote or line break inside brackets becomes
/// `_` too.
fn quoted_names(sql: &str) -> std::borrow::Cow<'_, str> {
    if !sql.contains(['[', '"']) {
        return sql.into();
    }
    let mut bytes = sql.as_bytes().to_vec();
    // Where what starts at `i` closes with `close` (doubled, it stands for
    // itself).
    let end = |bytes: &[u8], i: usize, close: &[u8], doubled: bool| {
        let mut j = i;
        while j < bytes.len() {
            if bytes[j..].starts_with(close) {
                if doubled && bytes.get(j + 1) == Some(&close[0]) {
                    j += 2;
                    continue;
                }
                return Some(j);
            }
            j += 1;
        }
        None
    };
    let mut i = 0;
    while i < bytes.len() {
        let next = match bytes[i] {
            b'\'' => end(&bytes, i + 1, b"'", true),
            b'`' => end(&bytes, i + 1, b"`", true),
            b'-' if bytes.get(i + 1) == Some(&b'-') => end(&bytes, i, b"\n", false),
            b'/' if bytes.get(i + 1) == Some(&b'*') => end(&bytes, i + 2, b"*/", false),
            open @ (b'"' | b'[') => {
                let close = if open == b'[' { b"]" } else { b"\"" };
                let found = end(&bytes, i + 1, close, true);
                if let Some(end) = found {
                    for b in &mut bytes[i + 1..end] {
                        if matches!(*b, b'"' | b'\n') {
                            *b = b'_';
                        }
                    }
                    bytes[i] = b'"';
                    bytes[end] = b'"';
                }
                found
            }
            _ => Some(i),
        };
        let Some(next) = next else { break };
        i = next + 1;
    }
    String::from_utf8(bytes)
        .expect("ASCII replaced by ASCII")
        .into()
}

/// `sql` lexed as `dialect` reads it: MySQL's `#` starts a comment, which
/// what is added after the select list must not land in; SQL Server's
/// `#t` is a name.
fn parse(dialect: Dialect, sql: &str) -> Option<Query> {
    let lexed = quoted_names(sql);
    let tokens: Vec<_> = tokenize_in(&lexed, dialect.syntax())
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let text = |i: usize| &sql[tokens[i].span.clone()];
    let is = |i: usize, word: &str| {
        tokens
            .get(i)
            .is_some_and(|t| t.kind.is_word() && text(i).eq_ignore_ascii_case(word))
    };
    let kind = |i: usize| tokens.get(i).map(|t| t.kind);
    // The parenthesis closing the one at `open`.
    let closing = |open: usize| {
        let mut depth = 0i32;
        (open..tokens.len()).find(|&j| {
            match tokens[j].kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth -= 1,
                _ => {}
            }
            depth == 0
        })
    };
    if !is(0, "SELECT") {
        return None;
    }
    let mut grouped = false;
    let mut depth = 0i32;
    let mut i = 1;
    let from = loop {
        match kind(i)? {
            // A subquery's aggregates are its own.
            TokenKind::LParen if is(i + 1, "SELECT") => i = closing(i)?,
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth -= 1,
            _ if depth == 0 && is(i, "FROM") => break i,
            _ if is(i, "DISTINCT") || is(i, "DISTINCTROW") => grouped = true,
            _ if kind(i + 1) == Some(TokenKind::LParen) && AGGREGATES.iter().any(|a| is(i, a)) => {
                grouped |= !is(closing(i + 1)? + 1, "OVER");
            }
            _ => {}
        }
        i += 1;
    };
    let list_end = tokens[from - 1].span.end;
    // The items, after hints and modifiers: SQL Server's TOP n, TOP (n),
    // PERCENT and WITH TIES too.
    let mut start = 1;
    loop {
        if kind(start) == Some(TokenKind::Hint) || LIST_MODIFIERS.iter().any(|m| is(start, m)) {
            start += 1;
        } else if is(start, "TOP") {
            start = match kind(start + 1) {
                Some(TokenKind::LParen) => closing(start + 1)? + 1,
                _ => start + 2,
            };
            if is(start, "PERCENT") {
                start += 1;
            }
            if is(start, "WITH") && is(start + 1, "TIES") {
                start += 2;
            }
        } else {
            break;
        }
    }
    // Names joined by dots, ending in a name or `*`.
    let item = |range: std::ops::Range<usize>| {
        let len = range.len();
        if len.is_multiple_of(2) {
            return Item::Other;
        }
        let mut found = Item::Column;
        for (k, j) in range.enumerate() {
            let fits = if k % 2 == 1 {
                kind(j) == Some(TokenKind::Dot)
            } else if k + 1 == len && kind(j) == Some(TokenKind::Operator) && text(j) == "*" {
                found = Item::Star;
                true
            } else {
                matches!(
                    kind(j),
                    Some(TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword)
                ) && !["NULL", "TRUE", "FALSE"].iter().any(|v| is(j, v))
            };
            if !fits {
                return Item::Other;
            }
        }
        found
    };
    let mut items = Vec::new();
    let mut item_start = start;
    let mut depth = 0i32;
    for (j, t) in tokens.iter().enumerate().take(from).skip(start) {
        match t.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth -= 1,
            TokenKind::Comma if depth == 0 => {
                items.push(item(item_start..j));
                item_start = j + 1;
            }
            _ => {}
        }
    }
    items.push(item(item_start..from));
    let i = from + 1;
    let ident = |i: usize| {
        tokens
            .get(i)
            .is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent))
    };
    let separator = |i: usize| match kind(i) {
        Some(TokenKind::Dot) => Some("."),
        Some(TokenKind::Operator) if text(i) == ".." => Some(".."),
        _ => None,
    };
    // A part of the table's name may be any word: the lexer's keywords are
    // Oracle's, and `public` or `session` are plain names elsewhere (#229,
    // #237). A word before `(` is a function (`table(…)`), `only t` not the
    // table.
    let part = |i: usize| {
        (ident(i) || kind(i) == Some(TokenKind::Keyword))
            && kind(i + 1) != Some(TokenKind::LParen)
            && !is(i, "ONLY")
    };
    if !part(i) {
        return None;
    }
    // `table`, `schema.table` or `database.schema.table`, also SQL
    // Server's `database..table` (the default schema). A name of four
    // parts (a linked server) isn't one to edit.
    let mut table = text(i).to_owned();
    let mut parts = 1;
    let mut next = i + 1;
    while let Some(separator) = separator(next) {
        parts += separator.len();
        if !part(next + 1) || parts > 3 {
            return None;
        }
        table = format!("{table}{separator}{}", text(next + 1));
        next += 2;
    }
    // Optional alias.
    if ident(next) {
        next += 1;
    }
    // Anything that brings in a second table makes the result read-only.
    let joins = [
        "JOIN",
        "INNER",
        "LEFT",
        "RIGHT",
        "FULL",
        "CROSS",
        "NATURAL",
        "STRAIGHT_JOIN",
        "UNION",
        "INTERSECT",
        "MINUS",
        "EXCEPT",
        // SQL Server's OUTER APPLY (CROSS APPLY has CROSS) and PIVOT: a
        // column of what they bring in was written to the table (#416).
        "OUTER",
        "APPLY",
        "PIVOT",
        "UNPIVOT",
    ];
    // From the table on: a join word that isn't a keyword looks like an alias.
    let multi = tokens.get(next).is_some_and(|t| t.kind == TokenKind::Comma)
        || (i + 1..tokens.len()).any(|j| joins.iter().any(|w| is(j, w)));
    if multi {
        return None;
    }
    let mut depth = 0i32;
    for (j, t) in tokens.iter().enumerate().skip(next) {
        match t.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth -= 1,
            _ if depth == 0 && (is(j, "GROUP") || is(j, "HAVING")) => grouped = true,
            _ => {}
        }
    }
    Some(Query {
        table,
        list_end,
        grouped,
        items,
    })
}

/// The table a simple single-table query reads from, as written.
pub fn single_table(dialect: Dialect, sql: &str) -> Option<String> {
    parse(dialect, sql).map(|q| q.table)
}

/// Whether the rows of `sql`'s result are rows of one table, so they can
/// be edited.
pub fn can_edit(dialect: Dialect, sql: &str) -> bool {
    parse(dialect, sql).is_some_and(|q| !q.grouped)
}

/// Which of the first `count` result columns of `sql` are columns of its
/// table as they are: `col` and `t.col`, and those `*` and `t.*` bring.
/// Editing writes a cell to the column of its name, which a column renamed
/// (`description as name`) or an expression isn't (#218). None is when the
/// select list doesn't match `count` columns.
pub fn plain_columns(dialect: Dialect, sql: &str, count: usize) -> Vec<bool> {
    let items = parse(dialect, sql.trim().trim_end_matches(';'))
        .map(|q| q.items)
        .unwrap_or_default();
    let stars = items.iter().filter(|i| **i == Item::Star).count();
    let others = items.len() - stars;
    // Each `*` brings the one table's columns.
    let per_star = match count.checked_sub(others) {
        Some(0) if stars == 0 => 0,
        Some(n) if stars > 0 && n % stars == 0 => n / stars,
        _ => return vec![false; count],
    };
    items
        .iter()
        .flat_map(|item| match item {
            Item::Star => vec![true; per_star],
            Item::Column => vec![true],
            Item::Other => vec![false],
        })
        .collect()
}

/// The columns that identify a row of `table` (as written in a query):
/// its primary key, or else a unique key on NOT NULL columns. Empty if it
/// has neither, or isn't a table (a view's indexes don't count).
pub fn row_key(client: &mut dyn Client, table: &str) -> Result<Vec<String>, DbError> {
    let keys = match client.dialect().kind() {
        // Rows are found by ROWID.
        DbKind::Oracle => Vec::new(),
        DbKind::MySql => mysql_keys(client, table)?,
        DbKind::Postgres => postgres_keys(client, table)?,
        DbKind::SqlServer => sqlserver_keys(client, table)?,
        DbKind::Sqlite => sqlite_keys(client, table)?,
    };
    Ok(keys
        .into_iter()
        .find_map(|(_, columns)| columns.into_iter().collect::<Option<Vec<_>>>())
        .unwrap_or_default())
}

/// A table's unique keys, the one to prefer first, each with its columns
/// in key order. A part that can't find a row is `None`: an expression, a
/// nullable column, or one that can't be compared.
type Keys = Vec<(String, Vec<Option<String>>)>;

/// Keys from (index, column) pairs in key order.
fn keys(parts: impl IntoIterator<Item = (String, Option<String>)>) -> Keys {
    let mut keys: Keys = Vec::new();
    for (index, column) in parts {
        match keys.last_mut() {
            Some((name, columns)) if *name == index => columns.push(column),
            _ => keys.push((index, vec![column])),
        }
    }
    keys
}

fn value(row: &[Cell], i: usize) -> Option<String> {
    row.get(i).cloned().flatten()
}

/// MySQL: SHOW INDEX finds the table as the query did, by the database in
/// use and the server's rules for the case of names. information_schema
/// would not: a temporary table isn't in it, and hides a table of its name
/// that is. A view has no indexes; a table SHOW INDEX can't read (no
/// privilege) has no key either.
fn mysql_keys(client: &mut dyn Client, table: &str) -> Result<Keys, DbError> {
    let quoted = |name: &str| format!("`{}`", name.replace('`', "``"));
    let table = match unquote(table).as_slice() {
        [schema, name] => format!("{}.{}", quoted(schema), quoted(name)),
        [name] => quoted(name),
        _ => return Ok(Vec::new()),
    };
    let rows = match client.query(&format!("show index from {table}"), &[]) {
        Ok(rows) => rows,
        Err(e) if e.code.is_some() => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    // A float's text is rounded: the row is found with a tolerance, which
    // finds its neighbours too (#417). Field, Type, …
    let approximate: std::collections::BTreeSet<String> = client
        .query(&format!("show columns from {table}"), &[])
        .unwrap_or_default()
        .iter()
        .filter(|row| {
            value(row, 1).is_some_and(|t| {
                let t = t.to_ascii_lowercase();
                ["float", "double", "real"].iter().any(|a| t.starts_with(a))
            })
        })
        .filter_map(|row| value(row, 0))
        .map(|c| c.to_lowercase())
        .collect();
    // Table, Non_unique, Key_name, Seq_in_index, Column_name, Collation,
    // Cardinality, Sub_part, Packed, Null: unique ones, the primary key
    // first.
    let mut parts: Vec<(String, u32, Option<String>)> = rows
        .iter()
        .filter(|row| value(row, 1).as_deref() == Some("0"))
        .map(|row| {
            let seq = value(row, 3).and_then(|s| s.parse().ok()).unwrap_or(0);
            // Functional key parts have no column; nullable ones and
            // floats don't identify a row.
            let column = value(row, 4)
                .filter(|_| value(row, 9).as_deref() != Some("YES"))
                .filter(|c| !approximate.contains(&c.to_lowercase()));
            (value(row, 2).unwrap_or_default(), seq, column)
        })
        .collect();
    parts.sort_by(|a, b| (a.0 != "PRIMARY", &a.0, a.1).cmp(&(b.0 != "PRIMARY", &b.0, b.1)));
    Ok(keys(
        parts.into_iter().map(|(index, _, column)| (index, column)),
    ))
}

/// PostgreSQL: `to_regclass` finds the table the way the query did, by
/// the search path, quotes and folding unquoted names to lowercase. Only
/// tables (partitioned too) count, and unique indexes that are valid and
/// cover every row (not partial); INCLUDE columns aren't part of the key.
fn postgres_keys(client: &mut dyn Client, table: &str) -> Result<Keys, DbError> {
    let rows = client.query(
        "select c.relname, a.attname, a.attnotnull and a.atttypid not in (700, 701) \
           from pg_index i \
           join pg_class t on t.oid = i.indrelid \
           join pg_class c on c.oid = i.indexrelid \
          cross join lateral unnest(i.indkey) with ordinality as k(attnum, n) \
           left join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum \
          where i.indrelid = to_regclass($1) \
            and t.relkind in ('r', 'p') \
            and i.indisunique and i.indisvalid and i.indpred is null \
            and k.n <= i.indnkeyatts \
          order by i.indisprimary desc, c.relname, k.n",
        &[table],
    )?;
    Ok(keys(rows.iter().map(|row| {
        // An expression has no column (attnum 0).
        let column = value(row, 1).filter(|_| value(row, 2).as_deref() == Some("t"));
        (value(row, 0).unwrap_or_default(), column)
    })))
}

/// SQLite: the table the query read, in the database it names or else the
/// first that has it (`temp`, `main`, then those attached), as SQLite looks
/// it up; a view has no key. Its primary key, its unique indexes on NOT
/// NULL columns (not partial ones), and last its `rowid`: every table but
/// one `WITHOUT ROWID` (which has a primary key) has one. A primary key
/// column that isn't NOT NULL may hold NULLs in a rowid table, an old
/// SQLite quirk, unless it is the rowid itself (`INTEGER PRIMARY KEY`).
fn sqlite_keys(client: &mut dyn Client, table: &str) -> Result<Keys, DbError> {
    let names = unquote(table);
    let (schema, name) = match names.as_slice() {
        [schema, name] => (Some(schema.clone()), name.clone()),
        [name] => (None, name.clone()),
        _ => return Ok(Vec::new()),
    };
    let schemas: Vec<String> = match schema {
        Some(schema) => vec![schema],
        None => client
            .query(
                "select name from pragma_database_list \
                  order by case name when 'temp' then 0 when 'main' then 1 else 2 end, seq",
                &[],
            )?
            .iter()
            .filter_map(|r| value(r, 0))
            .collect(),
    };
    let dialect = Dialect::SQLITE;
    let mut found = None;
    for schema in schemas {
        let rows = client.query(
            &format!(
                "select type, name, sql from {}.sqlite_schema \
                  where name = ? collate nocase and type in ('table', 'view')",
                dialect.quote_ident(&schema)
            ),
            &[&name],
        )?;
        if let Some(row) = rows.first() {
            found = Some((schema, row.clone()));
            break;
        }
    }
    let Some((schema, row)) = found else {
        return Ok(Vec::new());
    };
    if value(&row, 0).as_deref() != Some("table") {
        return Ok(Vec::new());
    }
    let table = value(&row, 1).unwrap_or(name);
    let sql = value(&row, 2).unwrap_or_default().to_uppercase();
    let rowid_table = !(sql.contains("WITHOUT") && sql.contains("ROWID"));
    let columns = client.query(
        "select name, \"notnull\", pk, upper(type) from pragma_table_info(?, ?) order by cid",
        &[&table, &schema],
    )?;
    // (position, name, NOT NULL, type) of the primary key's columns.
    let mut pk: Vec<(i64, Option<String>, bool, String)> = columns
        .iter()
        .filter_map(|c| {
            let position = value(c, 2)?.parse::<i64>().ok().filter(|p| *p > 0)?;
            let not_null = value(c, 1).as_deref() == Some("1");
            Some((
                position,
                value(c, 0),
                not_null,
                value(c, 3).unwrap_or_default(),
            ))
        })
        .collect();
    pk.sort_by_key(|(position, ..)| *position);
    // NULL-free: the rowid itself, a WITHOUT ROWID table's key, or NOT NULL.
    let rowid_alias = rowid_table && pk.len() == 1 && pk[0].3 == "INTEGER";
    let not_null: std::collections::BTreeSet<String> = columns
        .iter()
        .filter(|c| value(c, 1).as_deref() == Some("1"))
        .filter_map(|c| value(c, 0))
        .map(|n| n.to_lowercase())
        .collect();
    // A column of REAL affinity holds rounded floats, one of BLOB affinity
    // (or none) a 7 and a '7' alike: a lookup finds several rows (#417).
    let loose: std::collections::BTreeSet<String> = columns
        .iter()
        .filter(|c| loose_affinity(&value(c, 3).unwrap_or_default()))
        .filter_map(|c| value(c, 0))
        .map(|n| n.to_lowercase())
        .collect();
    let mut parts: Vec<(String, Option<String>)> = pk
        .into_iter()
        .map(|(_, column, not_null, _)| {
            let usable = (not_null || rowid_alias || !rowid_table)
                && (rowid_alias
                    || column
                        .as_ref()
                        .is_some_and(|c| !loose.contains(&c.to_lowercase())));
            ("primary key".to_owned(), column.filter(|_| usable))
        })
        .collect();
    let unique = client.query(
        "select il.name, ii.name from pragma_index_list(?1, ?2) il, \
                pragma_index_info(il.name, ?2) ii \
          where il.\"unique\" = 1 and il.partial = 0 and il.origin <> 'pk' \
          order by il.name, ii.seqno",
        &[&table, &schema],
    )?;
    parts.extend(unique.iter().map(|row| {
        // An expression has no column; a nullable one doesn't find a row.
        let column = value(row, 1)
            .filter(|c| not_null.contains(&c.to_lowercase()) && !loose.contains(&c.to_lowercase()));
        (value(row, 0).unwrap_or_default(), column)
    }));
    // Its names, unless a column has taken them.
    let names: Vec<String> = columns
        .iter()
        .filter_map(|c| value(c, 0))
        .map(|n| n.to_lowercase())
        .collect();
    let rowid = ["rowid", "_rowid_", "oid"]
        .into_iter()
        .find(|r| !names.iter().any(|n| n == r));
    if rowid_table && let Some(rowid) = rowid {
        parts.push(("rowid".to_owned(), Some(rowid.to_owned())));
    }
    Ok(keys(parts))
}

/// Whether a SQLite column of declared `type` has REAL or BLOB affinity,
/// by SQLite's rules (INT, then CHAR/CLOB/TEXT, then BLOB or none, then
/// REAL/FLOA/DOUB).
fn loose_affinity(declared: &str) -> bool {
    let t = declared.to_ascii_uppercase();
    if t.contains("INT") || ["CHAR", "CLOB", "TEXT"].iter().any(|a| t.contains(a)) {
        return false;
    }
    t.is_empty()
        || ["BLOB", "REAL", "FLOA", "DOUB"]
            .iter()
            .any(|a| t.contains(a))
}

/// SQL Server: `object_id` finds the table the way the query did, by the
/// default schema and the database's collation; in another database
/// (`db.schema.table`) its catalog has the indexes. Only tables count (an
/// indexed view's unique index doesn't), and unique indexes that are
/// enabled and not filtered. A `sql_variant` key can't be compared with
/// what was fetched: a value is compared as its base type's.
fn sqlserver_keys(client: &mut dyn Client, table: &str) -> Result<Keys, DbError> {
    let names = unquote(table);
    // A temporary table (`#t`, `##t`) is in tempdb, where object_id finds
    // it only by a name in tempdb.
    let (catalog, table) = match names.as_slice() {
        [name] if name.starts_with('#') => (
            "tempdb.".to_owned(),
            format!("tempdb..{}", Dialect::SQLSERVER.quote_ident(name)),
        ),
        [database, _, _] => (
            format!("{}.", Dialect::SQLSERVER.quote_ident(database)),
            table.to_owned(),
        ),
        _ => (String::new(), table.to_owned()),
    };
    let table = table.as_str();
    let sql = format!(
        "select i.name, c.name, c.is_nullable, c.system_type_id \
           from {catalog}sys.indexes i \
           join {catalog}sys.objects o on o.object_id = i.object_id \
           join {catalog}sys.index_columns ic \
             on ic.object_id = i.object_id and ic.index_id = i.index_id \
           join {catalog}sys.columns c \
             on c.object_id = ic.object_id and c.column_id = ic.column_id \
          where i.object_id = object_id(@P1) \
            and o.type = 'U' \
            and i.is_unique = 1 and i.has_filter = 0 \
            and i.is_disabled = 0 and i.is_hypothetical = 0 \
            and ic.key_ordinal > 0 \
          order by i.is_primary_key desc, i.name, ic.key_ordinal"
    );
    // Nor can a real or a float, read rounded (#417).
    const UNCOMPARABLE: [&str; 3] = ["98", "59", "62"];
    let rows = client.query(&sql, &[table])?;
    Ok(keys(rows.iter().map(|row| {
        let column = value(row, 1).filter(|_| {
            value(row, 2).as_deref() == Some("0")
                && !value(row, 3).is_some_and(|t| UNCOMPARABLE.contains(&t.as_str()))
        });
        (value(row, 0).unwrap_or_default(), column)
    })))
}

/// The names in `table`, `schema.table` or `database.schema.table`
/// without their quotes (`"…"`, `` `…` `` or `[…]`). SQL Server's
/// `database..table` has an empty schema.
fn unquote(name: &str) -> Vec<String> {
    let lexed = quoted_names(name);
    // Inside the quotes, a doubled closing quote stands for one.
    let inner = |text: &str| {
        let close = match text.chars().next()? {
            '[' => ']',
            quote => quote,
        };
        let body = text[1..].strip_suffix(close).unwrap_or(&text[1..]);
        Some(body.replace(&format!("{close}{close}"), &close.to_string()))
    };
    // SQL Server's temporary tables start with `#` (`##` global ones),
    // which isn't part of a word: the `#`s right before a word are its own.
    let mut hashes: Option<std::ops::Range<usize>> = None;
    tokenize(&lexed)
        .into_iter()
        .filter_map(|t| {
            let text = &name[t.span.clone()];
            let before = hashes.take().filter(|h| h.end == t.span.start);
            if !text.is_empty() && text.chars().all(|c| c == '#') {
                hashes = Some(before.map_or(t.span.start, |h| h.start)..t.span.end);
                return None;
            }
            let prefix = before.map_or("", |h| &name[h]);
            match t.kind {
                TokenKind::QuotedIdent => inner(text),
                TokenKind::Ident | TokenKind::Keyword => Some(format!("{prefix}{text}")),
                TokenKind::Operator if text == ".." => Some(String::new()),
                _ => None,
            }
        })
        .collect()
}

/// One DML statement plus the number of rows it must affect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dml {
    pub sql: String,
    pub expect_rows: u64,
    /// Human description for conflict messages, e.g. `row 12`.
    pub what: String,
    /// The change it applies.
    pub change: Change,
    /// Values for its bind variables: those loaded from files, and those
    /// Oracle takes no literal of (#293).
    pub binds: Vec<(String, Bind)>,
}

/// What an UPDATE or INSERT writes into `col`: a value loaded from a file
/// as the next of `binds`, which the statement names as returned (#293),
/// as is a value Oracle takes no literal of ([`oracle_bind`]); any other
/// as a literal, in an UPDATE of a value that `held` the one fetched. A
/// literal of megabytes would be impractical anywhere.
fn written(
    dialect: Dialect,
    col: &Column,
    value: Option<&str>,
    held: Option<&str>,
    file: Option<&Loaded>,
    binds: &mut Vec<(String, Bind)>,
) -> String {
    let bind = match (file, value) {
        (Some(loaded), _) => Some(loaded.bind()),
        (None, Some(v)) if dialect.kind() == DbKind::Oracle => oracle_bind(col, v),
        _ => None,
    };
    let Some(bind) = bind else {
        // An Oracle JSON keeps the scalars it holds where they stay (#370).
        if let (DbKind::Oracle, Some(v), Some(held)) = (dialect.kind(), value, held) {
            let name = dialect.quote_ident(&col.name);
            return pythia_data::sql_literal_replacing(col, v, &name, held);
        }
        return literal(dialect, col, value);
    };
    let name = format!("pythia_value{}", binds.len() + 1);
    let named = format!(":{name}");
    binds.push((name, bind));
    named
}

/// The parts of 1000 characters of a CLOB's literal (#317) that a value
/// may have before it is bound: Oracle parses many slowly, and failed
/// past about 250 with ORA-01489 until each part was a CLOB (#342).
const CLOB_LITERAL_PARTS: usize = 200;

/// A value typed into an Oracle cell that no literal holds, bound instead
/// (#293): bytes past the 2000 a `hextoraw` takes (#314) in a BLOB, RAW or
/// LONG RAW, text past 4000 bytes in a LONG (and in a CLOB or NCLOB,
/// which the pure-Rust driver types LONG), and a CLOB's or NCLOB's text
/// past [`CLOB_LITERAL_PARTS`]. `None` for any other, and for a binary
/// value that isn't hex, whose literal fails as it did. A JSON's or
/// VECTOR's text stays its literal of parts, of any length (#342): bound
/// as a LONG, it fails in a VECTOR (ORA-01461), and in a JSON next to
/// another LONG (ORA-40441).
fn oracle_bind(col: &Column, value: &str) -> Option<Bind> {
    let t = col.type_name.to_ascii_uppercase();
    if t.starts_with("RAW") || t == "BLOB" || t == "LONG RAW" {
        // Typed as the grid shows it, `0x…` is the bytes of its digits.
        let digits = match value.strip_prefix("0x") {
            Some(d) if d.len() % 2 == 1 => format!("0{d}"),
            Some(d) => d.to_owned(),
            None => value.to_owned(),
        };
        if digits.len() <= 2 * HEX_LITERAL_BYTES || !digits.len().is_multiple_of(2) {
            return None;
        }
        let bytes: Option<Vec<u8>> = (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok())
            .collect();
        return bytes.map(|b| Bind::Bytes(b.into()));
    }
    let long = match t.as_str() {
        "LONG" => value.len() > 4000,
        "CLOB" | "NCLOB" => value.encode_utf16().count() > CLOB_LITERAL_PARTS * 1000,
        _ => false,
    };
    long.then(|| Bind::LongText(value.into()))
}

/// A pending change of the grid, by its index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// A fetched row deleted.
    Delete(usize),
    /// A fetched row's cells changed.
    Update(usize),
    /// A new row.
    Insert(usize),
}

/// A row's changed cells: column → (value fetched, new value).
type Cells = BTreeMap<usize, (Cell, Cell)>;

/// The changes of an Apply that weren't made, by the key of their rows (the
/// result's last columns), to keep pending once the result is fetched
/// again (#247): those whose statement failed, found a conflict or didn't
/// run.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Unapplied {
    /// A row's key, its changed cells, and the files loaded into them.
    updates: Vec<(Vec<Cell>, Cells, Files)>,
    deletes: Vec<Vec<Cell>>,
    /// A new row, its files, and the columns set to NULL.
    inserts: Vec<(Vec<Cell>, Files, BTreeSet<usize>)>,
}

/// The values loaded from files into a row's cells, by column (#293).
type Files = BTreeMap<usize, Loaded>;

/// The files of `changes` loaded into `row`.
fn files_of(changes: &Changes, row: RowRef) -> Files {
    changes
        .loaded
        .range((row, 0)..=(row, usize::MAX))
        .map(|(&(_, c), loaded)| (c, loaded.clone()))
        .collect()
}

impl Unapplied {
    /// The `changes` to `rows` that had a statement (`planned`) but weren't
    /// `applied`; those without one are dropped, as an Apply drops them.
    pub fn new(
        changes: &Changes,
        planned: &[Change],
        applied: &[Change],
        rows: &[Vec<Cell>],
        key_start: usize,
    ) -> Self {
        let mut out = Self::default();
        let key = |r: usize| {
            rows.get(r)
                .map(|row| row[key_start.min(row.len())..].to_vec())
        };
        for change in planned.iter().filter(|c| !applied.contains(c)) {
            match *change {
                Change::Delete(r) => out.deletes.extend(key(r)),
                Change::Update(r) => {
                    let (Some(key), Some(cells)) = (key(r), changes.updates.get(&r)) else {
                        continue;
                    };
                    let cells = cells
                        .iter()
                        .map(|(&c, new)| (c, (value(&rows[r], c), new.clone())))
                        .collect();
                    out.updates
                        .push((key, cells, files_of(changes, RowRef::Existing(r))));
                }
                Change::Insert(i) => out.inserts.extend(changes.inserts.get(i).map(|row| {
                    let nulls = (0..row.len())
                        .filter(|&c| changes.nulls.contains(&(i, c)))
                        .collect();
                    (row.clone(), files_of(changes, RowRef::New(i)), nulls)
                })),
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deletes.is_empty() && self.inserts.is_empty()
    }

    /// The changes again, on `rows` (the result fetched again), and what
    /// wasn't kept: a row no longer among them, or one whose cells changed
    /// no longer have the values fetched. Another session changed those,
    /// which the next Apply would have reported as a conflict, and the
    /// change would now overwrite.
    pub fn keep(&self, rows: &[Vec<Cell>], key_start: usize) -> (Changes, Vec<String>) {
        let at: HashMap<&[Cell], usize> = rows
            .iter()
            .enumerate()
            .map(|(r, row)| (&row[key_start.min(row.len())..], r))
            .collect();
        let name = |key: &[Cell]| {
            key.iter()
                .map(|v| v.as_deref().unwrap_or("NULL"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut changes = Changes::default();
        let mut lost = Vec::new();
        for key in &self.deletes {
            match at.get(key.as_slice()) {
                Some(&r) => {
                    changes.deletes.insert(r);
                }
                None => lost.push(format!(
                    "The row with key {} isn't in the result any more: it wasn't deleted",
                    name(key)
                )),
            }
        }
        for (key, cells, files) in &self.updates {
            let Some(&r) = at.get(key.as_slice()) else {
                lost.push(format!(
                    "The row with key {} isn't in the result any more: its changes weren't kept",
                    name(key)
                ));
                continue;
            };
            if cells
                .iter()
                .any(|(&c, (fetched, _))| value(&rows[r], c) != *fetched)
            {
                lost.push(format!(
                    "The row with key {} was changed by another session: its changes weren't kept",
                    name(key)
                ));
                continue;
            }
            // A file's value stays, also where it shows as the value did.
            let cells: BTreeMap<usize, Cell> = cells
                .iter()
                .filter(|(c, (fetched, new))| fetched != new || files.contains_key(c))
                .map(|(&c, (_, new))| (c, new.clone()))
                .collect();
            if !cells.is_empty() {
                changes.updates.insert(r, cells);
            }
            for (&c, loaded) in files {
                changes
                    .loaded
                    .insert((RowRef::Existing(r), c), loaded.clone());
            }
        }
        for (i, (row, files, nulls)) in self.inserts.iter().enumerate() {
            changes.inserts.push(row.clone());
            changes.nulls.extend(nulls.iter().map(|&c| (i, c)));
            for (&c, loaded) in files {
                changes.loaded.insert((RowRef::New(i), c), loaded.clone());
            }
        }
        (changes, lost)
    }
}

/// PostgreSQL types compared with `=`: their text reads back as the value
/// shown. Others compare as text (see [`same`]).
const POSTGRES_EQUAL: &[&str] = &[
    "smallint",
    "integer",
    "bigint",
    "numeric",
    "oid",
    "money",
    "boolean",
    "text",
    "character varying",
    "character",
    "name",
    "char",
    "bytea",
    "date",
    "time",
    "time with time zone",
    "timestamp",
    "timestamp with time zone",
    "interval",
    "uuid",
    "inet",
    "cidr",
    "macaddr",
    "macaddr8",
    "bit",
    "varbit",
    "jsonb",
];

/// MySQL values longer than this (in bytes) are checked by their digest.
const LONG_VALUE: usize = 4096;

/// SQL that holds while MySQL column `name` (quoted) still has the long
/// `value`: its SHA-256, of the bytes a binary value's hex stands for, or
/// of the text in UTF-8, as it was fetched.
fn mysql_digest(name: &str, type_name: &str, value: &str) -> String {
    use sha2::{Digest, Sha256};
    let binary = matches!(
        type_name,
        "BINARY" | "VARBINARY" | "BLOB" | "BIT" | "GEOMETRY" | "VECTOR"
    );
    let hex =
        binary && value.len().is_multiple_of(2) && value.bytes().all(|b| b.is_ascii_hexdigit());
    let digest = if hex {
        let digit = |b: u8| (b as char).to_digit(16).unwrap_or_default() as u8;
        let bytes: Vec<u8> = value
            .as_bytes()
            .chunks(2)
            .map(|pair| (digit(pair[0]) << 4) | digit(pair[1]))
            .collect();
        Sha256::digest(&bytes)
    } else {
        Sha256::digest(value.as_bytes())
    };
    let digest: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if binary {
        format!("sha2({name}, 256) = '{digest}'")
    } else {
        format!("sha2(convert({name} using utf8mb4), 256) = '{digest}'")
    }
}

/// [`same`] on Oracle, for column `name` (quoted) and `value` as the
/// literal `lit`. DECODE treats NULL = NULL, but takes no LOB (ORA-00932)
/// and no LONG (ORA-00997), which no SQL can compare (#309): a CLOB, NCLOB
/// or BLOB is compared with `dbms_lob.compare` (an empty one by its
/// length, as `''` is NULL), a LONG or LONG RAW is left out of the check.
/// The pure-Rust driver types LOB columns LONG and LONG RAW, so they are
/// left out there too. Nor does DECODE take a JSON (ORA-40796) or a VECTOR
/// (ORA-00932) (#340): a JSON is compared as JSON with `json_equal`, which
/// takes the keys of an object in any order (Pythia shows them sorted); a
/// VECTOR, which SQL doesn't compare, is left out.
fn oracle_same(name: &str, col: &Column, value: Option<&str>, lit: &str) -> Option<String> {
    Some(match (col.type_name.as_str(), value) {
        ("LONG" | "LONG RAW" | "VECTOR", _) => return None,
        ("JSON", Some(_)) => format!("json_equal({name}, {lit})"),
        ("CLOB" | "NCLOB" | "BLOB" | "JSON", None) => format!("{name} is null"),
        ("CLOB" | "NCLOB" | "BLOB", Some("")) => format!("dbms_lob.getlength({name}) = 0"),
        ("BLOB", Some(v)) if v.len() > 2 * HEX_LITERAL_BYTES => long_blob_same(name, v)?,
        ("CLOB" | "NCLOB" | "BLOB", Some(_)) => format!("dbms_lob.compare({name}, {lit}) = 0"),
        _ => format!("decode({name}, {lit}, 1, 0) = 1"),
    })
}

/// The bytes a `hextoraw('…')` takes at most: its literal holds 4000
/// characters (ORA-01704).
const HEX_LITERAL_BYTES: usize = 2000;

/// SQL that holds while BLOB `name` still has the hex `value` of more
/// bytes than one `hextoraw` takes (#314): its length and its parts of
/// 2000 bytes, which `dbms_lob.substr` gives as RAWs. `None` for text
/// that isn't hex.
fn long_blob_same(name: &str, value: &str) -> Option<String> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let parts: Vec<String> = value
        .as_bytes()
        .chunks(2 * HEX_LITERAL_BYTES)
        .enumerate()
        .map(|(i, hex)| {
            let hex = std::str::from_utf8(hex).unwrap_or_default();
            let offset = i * HEX_LITERAL_BYTES + 1;
            format!("dbms_lob.substr({name}, {HEX_LITERAL_BYTES}, {offset}) = hextoraw('{hex}')")
        })
        .collect();
    Some(format!(
        "dbms_lob.getlength({name}) = {} and {}",
        value.len() / 2,
        parts.join(" and ")
    ))
}

/// SQL that holds while column `name` (of type `col`) still has `value`;
/// `None` if the column can't be compared with what was fetched.
fn same(dialect: Dialect, name: &str, col: &Column, value: Option<&str>) -> Option<String> {
    // A binary value held cut short (`… (5000 bytes)`) is only its first
    // 4000 bytes: no row has it (#223).
    if value.is_some_and(|v| pythia_db::cut_size(v).is_some()) {
        return None;
    }
    let name = dialect.quote_ident(name);
    let lit = literal(dialect, col, value);
    if dialect.kind() == DbKind::Oracle {
        return oracle_same(&name, col, value, &lit);
    }
    let Some(v) = value else {
        return Some(format!("{name} is null"));
    };
    let text = |type_name: &str| {
        let col = Column {
            name: String::new(),
            type_name: type_name.into(),
            numeric: false,
        };
        literal(dialect, &col, Some(v))
    };
    Some(match (dialect.kind(), col.type_name.as_str()) {
        // A long value is checked by its digest: written out, it made the
        // UPDATE twice as long as the value set (four times with a
        // backslash), longer than max_allowed_packet allows (#205).
        (DbKind::MySql, t) if v.len() > LONG_VALUE => mysql_digest(&name, t, v),
        // FLOAT is shown with six significant digits, rounded.
        (DbKind::MySql, "FLOAT") if v.parse::<f64>().is_ok_and(f64::is_finite) => {
            format!("abs({name} - {v}) <= abs({v}) * 0.00001")
        }
        // A string compares with JSON as a JSON string.
        (DbKind::MySql, "JSON") => format!("{name} = cast({lit} as json)"),
        // A VECTOR has no `=` (1210), its bytes have.
        (DbKind::MySql, "VECTOR") => format!("cast({name} as binary) = {lit}"),
        (DbKind::Postgres, t) if POSTGRES_EQUAL.contains(&t) => format!("{name} = {lit}"),
        // The others compare as the text that was shown: json, xml and
        // points have no `=`, a real compared with a number is widened to
        // double precision (where 0.1 isn't 0.1), arrays compare their
        // elements so, and the text also holds for floats shown rounded
        // (extra_float_digits below 1).
        (DbKind::Postgres, _) => format!("{name}::text = {}", text("text")),
        // A value compares as its base type's: as text, a date isn't one.
        (DbKind::SqlServer, "sql_variant") => return None,
        // text, ntext and xml can't be compared, their nvarchar(max) can.
        (DbKind::SqlServer, "text" | "ntext" | "xml") => {
            format!("cast({name} as nvarchar(max)) = {}", text("nvarchar"))
        }
        // The bytes, also those of CLR types (geometry, hierarchyid).
        (DbKind::SqlServer, "image" | "udt") => {
            let hex = v
                .strip_prefix("0x")
                .is_some_and(|h| h.chars().all(|c| c.is_ascii_hexdigit()));
            if !hex {
                return None;
            }
            format!("cast({name} as varbinary(max)) = {v}")
        }
        // 0.1 is a decimal, which a real takes as the real 0.1; as a float
        // (1E-1) the real would be widened instead.
        (DbKind::SqlServer, "real") => format!("{name} = cast({lit} as real)"),
        // A blob is shown as its hex, which text could be too.
        (DbKind::Sqlite, t)
            if t.contains("BLOB")
                && v.len() % 2 == 0
                && v.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            format!("{name} in (X'{v}', '{v}')")
        }
        // A column without a type holds `7` as a number or as text, which
        // look the same.
        (DbKind::Sqlite, _) if pythia_data::sqlite_number(v) => format!("{name} in ({v}, '{v}')"),
        _ => format!("{name} = {lit}"),
    })
}

/// A binary value held cut short: written, it would be its first 4000
/// bytes as text (#223). A cell set to one, pasted from a row, is left as
/// it is.
fn cut(value: &Cell) -> bool {
    value.as_deref().and_then(pythia_db::cut_size).is_some()
}

/// How many changed or new values [`dml`] leaves out because they are a
/// binary value cut short, as the grid holds it (pasted from another
/// cell, say): its first bytes aren't the value, so it isn't written.
pub fn cut_changes(edit: &Editable, columns: &[Column], changes: &Changes) -> usize {
    let key_start = columns.len().saturating_sub(edit.key.width());
    let skipped = |at: RowRef, c: usize, v: &Cell| {
        c < key_start && cut(v) && !changes.loaded.contains_key(&(at, c))
    };
    let updates = changes
        .updates
        .iter()
        .filter(|(r, _)| !changes.deletes.contains(r))
        .flat_map(|(&r, cells)| cells.iter().map(move |(&c, v)| (RowRef::Existing(r), c, v)));
    let inserts = changes.inserts.iter().enumerate().flat_map(|(i, row)| {
        row.iter()
            .enumerate()
            .map(move |(c, v)| (RowRef::New(i), c, v))
    });
    updates
        .chain(inserts)
        .filter(|&(at, c, v)| skipped(at, c, v))
        .count()
}

/// Where a fetched `row` of `edit`'s result is the row of its table, by
/// the key in its last columns; `None` without its key (the key
/// [`row_key`] picks can always be compared).
fn row_is(dialect: Dialect, edit: &Editable, columns: &[Column], row: &[Cell]) -> Option<String> {
    let key_start = columns.len().saturating_sub(edit.key.width());
    match &edit.key {
        RowKey::Rowid => value(row, key_start).map(|rowid| format!("rowid = '{rowid}'")),
        RowKey::Columns(key) => {
            let checks: Option<Vec<String>> = key
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let v = value(row, key_start + i)?;
                    same(dialect, name, &columns[key_start + i], Some(&v))
                })
                .collect();
            checks.map(|c| c.join(" and "))
        }
    }
}

/// The query that fetches the value of column `col` of a fetched `row` of
/// `edit`'s result again, by the row's key: *Save to file…* writes it
/// whole (#293).
pub fn value_query(
    dialect: Dialect,
    edit: &Editable,
    columns: &[Column],
    row: &[Cell],
    col: usize,
) -> Option<String> {
    let found = row_is(dialect, edit, columns, row)?;
    let name = dialect.quote_ident(&columns.get(col)?.name);
    Some(format!("select {name} from {} where {found}", edit.table))
}

/// Generates the DML for `changes` to the rows of `edit`, in `dialect`'s
/// SQL. The last columns of `columns` / `rows` hold the key.
pub fn dml(
    dialect: Dialect,
    edit: &Editable,
    columns: &[Column],
    rows: &[Vec<Cell>],
    changes: &Changes,
) -> Vec<Dml> {
    let mut out = Vec::new();
    let table = &edit.table;
    let key_start = columns.len().saturating_sub(edit.key.width());
    // Written under their names: the table's columns as they are (#218).
    let plain = plain_columns(dialect, &edit.source, key_start);
    let editable = |c: usize| plain.get(c).copied().unwrap_or(false);
    let row_is = |row: &[Cell]| row_is(dialect, edit, columns, row);

    for &r in &changes.deletes {
        let Some(row) = rows.get(r) else { continue };
        let Some(found) = row_is(row) else {
            continue;
        };
        out.push(Dml {
            sql: format!("delete from {table} where {found}"),
            expect_rows: 1,
            what: format!("row {}", r + 1),
            change: Change::Delete(r),
            binds: Vec::new(),
        });
    }
    for (&r, cells) in &changes.updates {
        if changes.deletes.contains(&r) {
            continue;
        }
        let Some(row) = rows.get(r) else { continue };
        let Some(found) = row_is(row) else {
            continue;
        };
        let file = |c: usize| changes.loaded.get(&(RowRef::Existing(r), c));
        let cells: Vec<_> = cells
            .iter()
            .filter(|(c, v)| editable(**c) && (file(**c).is_some() || !cut(v)))
            .collect();
        if cells.is_empty() {
            continue;
        }
        let mut binds = Vec::new();
        let sets: Vec<String> = cells
            .iter()
            .map(|(c, v)| {
                let col = &columns[**c];
                let held = row.get(**c).and_then(|v| v.as_deref());
                let value = written(dialect, col, v.as_deref(), held, file(**c), &mut binds);
                format!("{} = {value}", dialect.quote_ident(&col.name))
            })
            .collect();
        // A column that can't be compared is left out of the check.
        let checks: Vec<String> = std::iter::once(found)
            .chain(cells.iter().filter_map(|(c, _)| {
                let col = &columns[**c];
                same(
                    dialect,
                    &col.name,
                    col,
                    row.get(**c).and_then(|v| v.as_deref()),
                )
            }))
            .collect();
        out.push(Dml {
            sql: format!(
                "update {table} set {} where {}",
                sets.join(", "),
                checks.join(" and ")
            ),
            expect_rows: 1,
            what: format!("row {}", r + 1),
            change: Change::Update(r),
            binds,
        });
    }
    for (i, row) in changes.inserts.iter().enumerate() {
        let file = |c: usize| changes.loaded.get(&(RowRef::New(i), c));
        // A cell left NULL gets the column's default; one set NULL is NULL.
        let cells: Vec<(usize, Option<&str>)> = row
            .iter()
            .enumerate()
            .filter(|(c, v)| editable(*c) && (file(*c).is_some() || !cut(v)))
            .filter(|(c, v)| v.is_some() || changes.nulls.contains(&(i, *c)))
            .map(|(c, v)| (c, v.as_deref()))
            .collect();
        if cells.is_empty() {
            continue;
        }
        let names: Vec<String> = cells
            .iter()
            .map(|(c, _)| dialect.quote_ident(&columns[*c].name))
            .collect();
        let mut binds = Vec::new();
        let values: Vec<String> = cells
            .iter()
            .map(|(c, v)| written(dialect, &columns[*c], *v, None, file(*c), &mut binds))
            .collect();
        out.push(Dml {
            sql: format!(
                "insert into {table} ({}) values ({})",
                names.join(", "),
                values.join(", ")
            ),
            expect_rows: 1,
            what: format!("new row {}", i + 1),
            change: Change::Insert(i),
            binds,
        });
    }
    out
}

#[cfg(test)]
mod live;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn single_tables() {
        assert_eq!(
            single_table(Dialect::ORACLE, "select * from emp where x = 1"),
            Some("emp".into())
        );
        assert_eq!(
            single_table(Dialect::ORACLE, "select e.* from hr.emp e order by 1"),
            Some("hr.emp".into())
        );
        assert_eq!(
            single_table(
                Dialect::ORACLE,
                "select (select 1 from dual) x from \"Mixed\" t"
            ),
            Some("\"Mixed\"".into())
        );
        assert_eq!(
            single_table(Dialect::MYSQL, "select * from `shop`.`order` o"),
            Some("`shop`.`order`".into())
        );
        assert_eq!(
            single_table(
                Dialect::ORACLE,
                "select * from emp e join dept d on d.id = e.dept"
            ),
            None
        );
        assert_eq!(
            single_table(Dialect::ORACLE, "select * from emp, dept"),
            None
        );
        // A doubled quote is part of the name, not the start of an alias.
        assert_eq!(
            single_table(Dialect::ORACLE, "select * from s.\"a\"\"b\" x"),
            Some("s.\"a\"\"b\"".into())
        );
        assert_eq!(
            single_table(Dialect::ORACLE, "select * from emp straight_join dept"),
            None
        );
        assert_eq!(single_table(Dialect::ORACLE, "update emp set x = 1"), None);
        // A part named like a keyword: PostgreSQL's public, as the object
        // browser queries a table, a MySQL table named user (#237). A word
        // before `(` is no table: Oracle's TABLE(…).
        assert_eq!(
            single_table(
                Dialect::ORACLE,
                "select * from public.\"guidelines-division\""
            ),
            Some("public.\"guidelines-division\"".into())
        );
        assert_eq!(
            single_table(
                Dialect::ORACLE,
                "select * from public.orders o where o.id = 1"
            ),
            Some("public.orders".into())
        );
        assert!(can_edit(Dialect::ORACLE, "select * from public.orders"));
        assert_eq!(
            single_table(Dialect::ORACLE, "select * from shop.user"),
            Some("shop.user".into())
        );
        assert_eq!(
            single_table(Dialect::ORACLE, "select * from table(f())"),
            None
        );
    }

    /// Names that are words of Oracle's, which other databases have as
    /// plain names: PostgreSQL's schema public, as *Query data* writes it,
    /// and tables such as session or comment.
    #[test]
    fn single_tables_named_like_keywords() {
        assert_eq!(
            single_table(Dialect::POSTGRES, "select * from public.\"Orders\""),
            Some("public.\"Orders\"".into())
        );
        assert_eq!(
            single_table(
                Dialect::POSTGRES,
                "select * from public.session s where s.id = 1"
            ),
            Some("public.session".into())
        );
        assert_eq!(
            single_table(Dialect::POSTGRES, "select * from comment"),
            Some("comment".into())
        );
        assert_eq!(
            single_table(Dialect::POSTGRES, "select c.* from shop.type c order by 1"),
            Some("shop.type".into())
        );
        assert!(can_edit(Dialect::POSTGRES, "select * from public.emp"));
        assert_eq!(
            keyed_query(
                Dialect::POSTGRES,
                "select * from public.emp",
                &["id".into()]
            ),
            Some("select *, id as pythia$id from public.emp".into())
        );
        assert_eq!(
            plain_columns(Dialect::POSTGRES, "select id, level from public.session", 2),
            [true, true]
        );
        // What follows FROM without being a table.
        assert_eq!(
            single_table(Dialect::POSTGRES, "select * from table(f(1))"),
            None
        );
        assert_eq!(
            single_table(Dialect::POSTGRES, "select * from only emp"),
            None
        );
        assert_eq!(
            single_table(
                Dialect::POSTGRES,
                "select * from public.emp join type t on t.id = 1"
            ),
            None
        );
    }

    /// SQL Server's bracketed names, and names of a table in another
    /// database.
    #[test]
    fn single_tables_on_sql_server() {
        assert_eq!(
            single_table(
                Dialect::SQLSERVER,
                "select [order], t.[x y] from [dbo].[a]]b] t where n = ']'"
            ),
            Some("[dbo].[a]]b]".into())
        );
        assert_eq!(
            single_table(
                Dialect::SQLSERVER,
                "select top 10 * from shop.dbo.orders with (nolock)"
            ),
            Some("shop.dbo.orders".into())
        );
        assert_eq!(
            single_table(Dialect::SQLSERVER, "select * from shop..orders"),
            Some("shop..orders".into())
        );
        assert_eq!(
            single_table(Dialect::SQLSERVER, "select * from [it's \"odd\"] -- [x]"),
            Some("[it's \"odd\"]".into())
        );
        // What APPLY or PIVOT brings in isn't the table's.
        for sql in [
            "select * from o outer apply (select top 1 n.note from n where n.oid = o.id) x",
            "select * from o cross apply f(o.id) x",
            "select * from s pivot (sum(v) for k in ([a], [b])) p",
            "select * from s unpivot (v for k in (a, b)) u",
        ] {
            assert_eq!(single_table(Dialect::SQLSERVER, sql), None, "{sql}");
        }
        // A temporary table.
        assert_eq!(
            single_table(Dialect::SQLSERVER, "select * from #orders o"),
            Some("#orders".into())
        );
        assert_eq!(
            single_table(Dialect::SQLSERVER, "select * from ##orders"),
            Some("##orders".into())
        );
        // A linked server's table, a name cut short.
        assert_eq!(
            single_table(Dialect::SQLSERVER, "select * from srv.shop.dbo.orders"),
            None
        );
        assert_eq!(single_table(Dialect::SQLSERVER, "select * from dbo."), None);
        // PostgreSQL's arrays aren't names.
        assert_eq!(
            single_table(Dialect::POSTGRES, "select a[1], b[2:3] from t"),
            Some("t".into())
        );
        assert!(can_edit(Dialect::POSTGRES, "select a[1] from t"));
    }

    #[test]
    fn grouped_results_cant_be_edited() {
        for sql in [
            "select distinct dept from emp",
            "select count(*) from emp",
            "select coalesce(max(sal), 0) from emp where dept = 1",
            "select dept, sum(sal) from emp group by dept",
            "select dept from emp group by dept having count(*) > 1",
            "select * from emp group by id with rollup",
        ] {
            assert!(!can_edit(Dialect::ORACLE, sql), "{sql}");
            assert!(single_table(Dialect::ORACLE, sql).is_some(), "{sql}");
        }
        for sql in [
            "select * from emp",
            "select e.*, (select max(sal) from emp) top from emp e",
            "select e.*, sum(sal) over (partition by dept) from emp e",
            "select extract(year from hired) from emp where id in (select id from x group by id)",
            "select * from emp where name = 'distinct' -- count(*)",
        ] {
            assert!(can_edit(Dialect::ORACLE, sql), "{sql}");
        }
    }

    /// The result columns editing writes to, under their names: the
    /// table's columns as they are (#218).
    #[test]
    fn plain_columns_of_select_lists() {
        assert_eq!(
            plain_columns(
                Dialect::ORACLE,
                "select id, description as name, name as title from t",
                3
            ),
            [true, false, false]
        );
        assert_eq!(
            plain_columns(
                Dialect::ORACLE,
                "select t.id, t.\"Name\", s.t.x, [a b], `c` from s.t t",
                5
            ),
            [true; 5]
        );
        assert_eq!(
            plain_columns(Dialect::ORACLE, "select *, upper(name) u from t", 4),
            [true, true, true, false]
        );
        assert_eq!(
            plain_columns(Dialect::ORACLE, "select t.*, 1, t.* from t;", 5),
            [true, true, false, true, true]
        );
        assert_eq!(
            plain_columns(
                Dialect::ORACLE,
                "select top (5) percent with ties name from t",
                1
            ),
            [true]
        );
        assert_eq!(
            plain_columns(
                Dialect::ORACLE,
                "select /*+ full(t) */ distinct name, id from t",
                2
            ),
            [true, true]
        );
        assert_eq!(
            plain_columns(
                Dialect::POSTGRES,
                "select name n, -id, (id), id::text, null, t.id + 1, a[1], name = x from t",
                8
            ),
            [false; 8]
        );
        // A select list that doesn't match the result: none.
        assert_eq!(
            plain_columns(Dialect::ORACLE, "select id, name from t", 3),
            [false; 3]
        );
        assert_eq!(
            plain_columns(Dialect::ORACLE, "select *, *, x from t", 4),
            [false; 4]
        );
        assert_eq!(
            plain_columns(Dialect::ORACLE, "update t set x = 1", 1),
            [false]
        );
    }

    #[test]
    fn keyed_queries() {
        let key = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            keyed_query(
                Dialect::MYSQL,
                "select * from emp where x = 1;",
                &key(&["id"])
            )
            .as_deref(),
            Some("select *, id as pythia$id from emp where x = 1")
        );
        assert_eq!(
            keyed_query(
                Dialect::MYSQL,
                "select e.name -- the name\n  from `order` e",
                &key(&["order id", "line"])
            )
            .as_deref(),
            Some(
                "select e.name, `order id` as `pythia$order id`, line as pythia$line \
                 -- the name\n  from `order` e"
            )
        );
        assert_eq!(
            keyed_query(Dialect::MYSQL, "select count(*) from emp", &key(&["id"])),
            None
        );
        // MySQL's `#` comment ends at the line's end: the key goes before
        // it, not into it. `--x` is no comment there: `- -x`.
        assert_eq!(
            keyed_query(
                Dialect::MYSQL,
                "select id, parent_id # the columns\nfrom t",
                &key(&["id"])
            )
            .as_deref(),
            Some("select id, parent_id, id as pythia$id # the columns\nfrom t")
        );
        assert_eq!(
            keyed_query(Dialect::MYSQL, "select a --x\nfrom t", &key(&["id"])).as_deref(),
            Some("select a --x, id as pythia$id\nfrom t")
        );
        // Each database quotes its own way.
        assert_eq!(
            keyed_query(
                Dialect::POSTGRES,
                "select * from \"Order\" o",
                &key(&["Id", "line"])
            )
            .as_deref(),
            Some("select *, \"Id\" as \"pythia$Id\", line as pythia$line from \"Order\" o")
        );
        assert_eq!(
            keyed_query(
                Dialect::SQLSERVER,
                "select top 5 name from [dbo].[Order Lines]",
                &key(&["order id"])
            )
            .as_deref(),
            Some("select top 5 name, [order id] as [pythia$order id] from [dbo].[Order Lines]")
        );
    }

    /// Save to file fetches a value again by its row's key (#293).
    #[test]
    fn values_are_fetched_by_their_rows_key() {
        let oracle = Editable {
            table: "docs".into(),
            source: "select * from docs".into(),
            key: RowKey::Rowid,
        };
        let columns = vec![
            col("ID", "NUMBER", true),
            col("DOC", "BLOB", false),
            col(ROWID_COLUMN, "ROWID", false),
        ];
        let row = vec![Some("1".into()), None, Some("AAAx".into())];
        assert_eq!(
            value_query(Dialect::ORACLE, &oracle, &columns, &row, 1).as_deref(),
            Some("select DOC from docs where rowid = 'AAAx'")
        );
        let keyed = Editable {
            table: "[dbo].[Docs]".into(),
            source: "select * from [dbo].[Docs]".into(),
            key: RowKey::Columns(vec!["doc id".into()]),
        };
        let columns = vec![
            col("Body Text", "nvarchar", false),
            col("pythia$doc id", "int", true),
        ];
        let row = vec![Some("x".into()), Some("7".into())];
        assert_eq!(
            value_query(Dialect::SQLSERVER, &keyed, &columns, &row, 0).as_deref(),
            Some("select [Body Text] from [dbo].[Docs] where [doc id] = 7")
        );
        // Without its key, a row isn't found.
        let row = vec![Some("x".into()), None];
        assert_eq!(
            value_query(Dialect::SQLSERVER, &keyed, &columns, &row, 0),
            None
        );
    }

    #[test]
    fn unquoting() {
        assert_eq!(unquote("emp"), ["emp"]);
        assert_eq!(unquote("`my db`.`a``b`"), ["my db", "a`b"]);
        assert_eq!(unquote("\"Mixed\".x"), ["Mixed", "x"]);
        assert_eq!(unquote("\"a\"\"b\""), ["a\"b"]);
        assert_eq!(unquote("[dbo].[a]]b]"), ["dbo", "a]b"]);
        assert_eq!(unquote("[it's \"x\"]"), ["it's \"x\""]);
        // SQL Server's temporary tables.
        assert_eq!(unquote("#t"), ["#t"]);
        assert_eq!(unquote("##g_1"), ["##g_1"]);
        assert_eq!(unquote("[#t x]"), ["#t x"]);
        assert_eq!(unquote("shop..orders"), ["shop", "", "orders"]);
        assert_eq!(unquote("shop.[dbo].orders"), ["shop", "dbo", "orders"]);
    }

    fn col(n: &str, t: &str, numeric: bool) -> Column {
        Column {
            name: n.into(),
            type_name: t.into(),
            numeric,
        }
    }

    /// A result edited must end with the key columns its query added.
    #[test]
    fn key_columns_at_the_end() {
        let edit = |key: RowKey| Editable {
            table: "t".into(),
            source: "select id, parent_id from t".into(),
            key,
        };
        let id = edit(RowKey::Columns(vec!["id".into()]));
        let columns =
            |names: &[&str]| -> Vec<Column> { names.iter().map(|n| col(n, "INT", true)).collect() };
        assert!(has_key_columns(
            &id,
            &columns(&["id", "parent_id", "pythia$id"])
        ));
        // The key added inside a comment: the last column isn't it.
        assert!(!has_key_columns(&id, &columns(&["id", "parent_id"])));
        assert!(!has_key_columns(&id, &columns(&[])));
        let both = edit(RowKey::Columns(vec!["Order Id".into(), "line".into()]));
        assert!(has_key_columns(
            &both,
            &columns(&["x", "pythia$Order Id", "pythia$line"])
        ));
        assert!(!has_key_columns(
            &both,
            &columns(&["pythia$line", "pythia$Order Id"])
        ));
        let rowid = edit(RowKey::Rowid);
        assert!(has_key_columns(&rowid, &columns(&["ID", "PYTHIA$ROWID"])));
        // PostgreSQL's names end at 63 bytes.
        let long = "k".repeat(60);
        let cut = format!("pythia${long}")[..63].to_owned();
        assert!(has_key_columns(
            &edit(RowKey::Columns(vec![long])),
            &columns(&["x", &cut])
        ));
    }

    /// A new row's cell set NULL on purpose is written as NULL; one left
    /// alone is left out, for the column's default (#262).
    #[test]
    fn new_rows_write_the_nulls_they_were_given() {
        let columns = vec![
            col("id", "INT", true),
            col("status", "VARCHAR", false),
            col("note", "VARCHAR", false),
            col("pythia$id", "INT", true),
        ];
        let edit = Editable {
            table: "t".into(),
            source: "select id, status, note from t".into(),
            key: RowKey::Columns(vec!["id".into()]),
        };
        let changes = Changes {
            inserts: vec![vec![Some("2".into()), None, None, None]],
            nulls: BTreeSet::from([(0, 1)]),
            ..Changes::default()
        };
        let sql = dml(Dialect::MYSQL, &edit, &columns, &[], &changes);
        assert_eq!(sql[0].sql, "insert into t (id, status) values (2, NULL)");
    }

    #[test]
    fn generates_dml() {
        let columns = vec![
            col("ID", "NUMBER", true),
            col("NAME", "VARCHAR2(10)", false),
            col(ROWID_COLUMN, "ROWID", false),
        ];
        let rows = vec![
            vec![Some("1".into()), Some("a".into()), Some("AAA1".into())],
            vec![Some("2".into()), None, Some("AAA2".into())],
        ];
        let changes = Changes {
            updates: BTreeMap::from([(1, BTreeMap::from([(1, Some("b'c".into()))]))]),
            inserts: vec![vec![Some("3".into()), None, None]],
            deletes: [0].into(),
            ..Changes::default()
        };
        let edit = Editable {
            table: "emp".into(),
            source: "select * from emp".into(),
            key: RowKey::Rowid,
        };
        let sql: Vec<String> = dml(Dialect::ORACLE, &edit, &columns, &rows, &changes)
            .into_iter()
            .map(|d| d.sql)
            .collect();
        assert_eq!(
            sql,
            [
                "delete from emp where rowid = 'AAA1'",
                "update emp set NAME = 'b''c' where rowid = 'AAA2' and decode(NAME, NULL, 1, 0) = 1",
                "insert into emp (ID) values (3)",
            ]
        );
    }

    /// What an Apply that stopped part-way didn't make stays pending on the
    /// rows fetched again (#247), on Oracle by ROWID: row 3's delete and
    /// row 1's update went through, row 2's failed and row 4's didn't run.
    /// A row another session changed loses its change, as does one that's
    /// gone; the new row stays new.
    #[test]
    fn unapplied_changes_stay_pending() {
        let row = |id: &str, name: &str, rowid: &str| {
            vec![
                Some(id.to_owned()),
                Some(name.to_owned()),
                Some(rowid.to_owned()),
            ]
        };
        let columns = vec![
            col("ID", "NUMBER", true),
            col("NAME", "VARCHAR2(10)", false),
            col(ROWID_COLUMN, "ROWID", false),
        ];
        let rows = vec![
            row("1", "a", "AAA1"),
            row("2", "b", "AAA2"),
            row("3", "c", "AAA3"),
            row("4", "d", "AAA4"),
            row("5", "e", "AAA5"),
        ];
        let set = |v: &str| BTreeMap::from([(1, Some(v.to_owned()))]);
        let changes = Changes {
            updates: BTreeMap::from([(0, set("x")), (1, set("y")), (3, set("z")), (4, set("w"))]),
            inserts: vec![vec![Some("6".into()), Some("n".into()), None]],
            deletes: [2].into(),
            ..Changes::default()
        };
        let edit = Editable {
            table: "emp".into(),
            source: "select * from emp".into(),
            key: RowKey::Rowid,
        };
        let planned: Vec<Change> = dml(Dialect::ORACLE, &edit, &columns, &rows, &changes)
            .into_iter()
            .map(|d| d.change)
            .collect();
        assert_eq!(
            planned,
            [
                Change::Delete(2),
                Change::Update(0),
                Change::Update(1),
                Change::Update(3),
                Change::Update(4),
                Change::Insert(0),
            ]
        );
        let applied = [Change::Delete(2), Change::Update(0)];
        let unapplied = Unapplied::new(&changes, &planned, &applied, &rows, 2);
        assert!(!unapplied.is_empty());
        // Fetched again: 3 is gone, 1 has its new value, another session
        // changed 4 and deleted 5, and the rows come in another order.
        let again = vec![
            row("4", "theirs", "AAA4"),
            row("2", "b", "AAA2"),
            row("1", "x", "AAA1"),
        ];
        let (kept, lost) = unapplied.keep(&again, 2);
        assert_eq!(kept.updates, BTreeMap::from([(1, set("y"))]));
        assert_eq!(kept.inserts, changes.inserts);
        assert!(kept.deletes.is_empty());
        assert_eq!(
            lost,
            [
                "The row with key AAA4 was changed by another session: its changes weren't kept",
                "The row with key AAA5 isn't in the result any more: its changes weren't kept",
            ]
        );
        // Everything applied: nothing to keep.
        assert!(Unapplied::new(&changes, &planned, &planned, &rows, 2).is_empty());
    }

    /// A value loaded from a file is bound whole, not written as a literal,
    /// also when it shows cut short; an Apply that didn't make it keeps it
    /// pending, the file's value with it (#293).
    #[test]
    fn files_are_bound() {
        let columns = vec![
            col("id", "INT", true),
            col("b", "BLOB", false),
            col("t", "TEXT", false),
            col("pythia$id", "INT", true),
        ];
        let rows = vec![vec![
            Some("1".into()),
            None,
            Some("x".into()),
            Some("1".into()),
        ]];
        let bytes = Loaded::Bytes(vec![7; 5000].into());
        let text = Loaded::Text("é".repeat(3000).into());
        let shown = |l: &Loaded| Some(l.shown(Dialect::MYSQL));
        let changes = Changes {
            updates: BTreeMap::from([(0, BTreeMap::from([(1, shown(&bytes)), (2, shown(&text))]))]),
            inserts: vec![vec![Some("2".into()), shown(&bytes), None, None]],
            loaded: BTreeMap::from([
                ((RowRef::Existing(0), 1), bytes.clone()),
                ((RowRef::Existing(0), 2), text.clone()),
                ((RowRef::New(0), 1), bytes.clone()),
            ]),
            ..Changes::default()
        };
        let edit = Editable {
            table: "docs".into(),
            source: "select * from docs".into(),
            key: RowKey::Columns(vec!["id".into()]),
        };
        let statements = dml(Dialect::MYSQL, &edit, &columns, &rows, &changes);
        let sql: Vec<&str> = statements.iter().map(|d| d.sql.as_str()).collect();
        assert_eq!(
            sql,
            [
                "update docs set b = :pythia_value1, t = :pythia_value2 \
                 where id = 1 and b is null and t = 'x'",
                "insert into docs (id, b) values (2, :pythia_value1)",
            ]
        );
        assert_eq!(
            statements[0].binds,
            [
                ("pythia_value1".to_owned(), bytes.bind()),
                ("pythia_value2".to_owned(), text.bind())
            ]
        );
        assert_eq!(
            statements[1].binds,
            [("pythia_value1".to_owned(), bytes.bind())]
        );

        let planned: Vec<Change> = statements.iter().map(|d| d.change).collect();
        let unapplied = Unapplied::new(&changes, &planned, &[], &rows, 3);
        let (kept, lost) = unapplied.keep(&rows, 3);
        assert!(lost.is_empty(), "{lost:?}");
        assert_eq!(kept, changes);
    }

    /// On Oracle, a value typed that no literal holds is bound too (#293):
    /// bytes past 2000 in a BLOB, RAW or LONG RAW, text past 4000 bytes in
    /// a LONG (a CLOB as the pure-Rust driver types it), and a CLOB's past
    /// the 200 parts of its literal; shorter ones stay literals.
    #[test]
    fn oracle_binds_what_no_literal_holds() {
        let bytes = |n: usize| "AB".repeat(n);
        let cases = [
            ("BLOB", bytes(2000), false),
            ("BLOB", bytes(2001), true),
            ("BLOB", format!("0x{}", bytes(3000)), true),
            ("BLOB", format!("0x{}B", bytes(3000)), true),
            ("RAW(2000)", bytes(2001), true),
            ("LONG RAW", bytes(5000), true),
            // Not hex: its literal fails as it did.
            ("BLOB", "xy".repeat(3000), false),
            ("LONG", "é".repeat(2000), false),
            ("LONG", "é".repeat(2001), true),
            ("CLOB", "é".repeat(150_000), false),
            ("NCLOB", "x".repeat(200_001), true),
            ("VARCHAR2(4000)", "x".repeat(5000), false),
        ];
        for (type_name, value, bound) in cases {
            let bind = oracle_bind(&col("C", type_name, false), &value);
            assert_eq!(bind.is_some(), bound, "{type_name} of {}", value.len());
        }
        let blob = col("B", "BLOB", false);
        assert_eq!(
            oracle_bind(&blob, &format!("0x{}1", "00".repeat(2000))),
            Some(Bind::Bytes([vec![0; 2000], vec![1]].concat().into()))
        );

        let columns = vec![
            col("ID", "NUMBER", true),
            blob,
            col(ROWID_COLUMN, "ROWID", false),
        ];
        let rows = vec![vec![Some("1".into()), None, Some("AAA1".into())]];
        let changes = Changes {
            updates: BTreeMap::from([(0, BTreeMap::from([(1, Some(bytes(3000)))]))]),
            inserts: vec![vec![Some("2".into()), Some("CAFE".into()), None]],
            ..Changes::default()
        };
        let edit = Editable {
            table: "docs".into(),
            source: "select * from docs".into(),
            key: RowKey::Rowid,
        };
        let statements = dml(Dialect::ORACLE, &edit, &columns, &rows, &changes);
        let sql: Vec<&str> = statements.iter().map(|d| d.sql.as_str()).collect();
        assert_eq!(
            sql,
            [
                "update docs set B = :pythia_value1 where rowid = 'AAA1' and B is null",
                "insert into docs (ID, B) values (2, hextoraw('CAFE'))",
            ]
        );
        assert_eq!(statements[0].binds.len(), 1);
        assert!(statements[1].binds.is_empty());
    }

    #[test]
    fn generates_mysql_dml() {
        let columns = vec![
            col("id", "INT", true),
            col("line", "INT", true),
            col("note", "VARCHAR", false),
            col("price", "FLOAT", true),
            col("doc", "JSON", false),
            col("pythia$id", "INT", true),
            col("pythia$line", "INT", true),
        ];
        let row = |id: &str, note: Option<&str>| {
            vec![
                Some(id.into()),
                Some("1".into()),
                note.map(Into::into),
                Some("123457".into()),
                Some(r#"{"a": 1}"#.into()),
                Some(id.into()),
                Some("1".into()),
            ]
        };
        let rows = vec![row("1", Some("a")), row("2", None)];
        let changes = Changes {
            updates: BTreeMap::from([
                (
                    1,
                    BTreeMap::from([(2, Some(r"C:\temp".into())), (5, Some("9".into()))]),
                ),
                (
                    0,
                    BTreeMap::from([(3, Some("1.5".into())), (4, Some("[]".into()))]),
                ),
            ]),
            inserts: vec![vec![None, Some("2".into()), Some("it's".into())]],
            ..Changes::default()
        };
        let edit = Editable {
            table: "`order`".into(),
            source: "select * from `order`".into(),
            key: RowKey::Columns(vec!["id".into(), "line".into()]),
        };
        let sql: Vec<String> = dml(Dialect::MYSQL, &edit, &columns, &rows, &changes)
            .into_iter()
            .map(|d| d.sql)
            .collect();
        assert_eq!(
            sql,
            [
                "update `order` set price = 1.5, doc = '[]' where id = 1 and line = 1 \
                 and abs(price - 123457) <= abs(123457) * 0.00001 \
                 and doc = cast('{\"a\": 1}' as json)",
                "update `order` set note = _utf8mb4 X'433A5C74656D70' where id = 2 and line = 1 \
                 and note is null",
                "insert into `order` (line, note) values (2, 'it''s')",
            ]
        );
        let deletes = Changes {
            deletes: [1].into(),
            ..Changes::default()
        };
        assert_eq!(
            dml(Dialect::MYSQL, &edit, &columns, &rows, &deletes)[0].sql,
            "delete from `order` where id = 2 and line = 1"
        );
    }

    /// Changes in `dialect`'s SQL: the new value of every editable column
    /// but the first and a delete, for a row whose columns are `row`.
    fn dml_for(
        dialect: Dialect,
        columns: &[Column],
        row: Vec<Cell>,
        values: &[&str],
    ) -> Vec<String> {
        let edit = Editable {
            table: "t".into(),
            source: "select * from t".into(),
            key: RowKey::Columns(vec!["id".into()]),
        };
        let updates = values
            .iter()
            .enumerate()
            .map(|(c, v)| (c + 1, Some((*v).to_owned())))
            .collect();
        let changes = Changes {
            updates: BTreeMap::from([(0, updates)]),
            ..Changes::default()
        };
        let rows = vec![row];
        let mut sql: Vec<String> = dml(dialect, &edit, columns, &rows, &changes)
            .into_iter()
            .map(|d| d.sql)
            .collect();
        let deletes = Changes {
            deletes: [0].into(),
            ..Changes::default()
        };
        sql.extend(
            dml(dialect, &edit, columns, &rows, &deletes)
                .into_iter()
                .map(|d| d.sql),
        );
        sql
    }

    #[test]
    fn generates_postgres_dml() {
        let columns = vec![
            col("id", "integer", true),
            col("Note", "text", false),
            col("r", "real", true),
            col("doc", "json", false),
            col("x", "xml", false),
            col("tags", "text[]", false),
            col("ok", "boolean", false),
            col("at", "timestamp with time zone", false),
            col("pythia$id", "integer", true),
        ];
        let row = [
            "7",
            r"C:\temp",
            "0.1",
            r#"{"a": 1}"#,
            "<a/>",
            "{x,y}",
            "t",
            "2026-01-02 03:04:05.123456+01",
            "7",
        ]
        .map(|v| Some(v.to_owned()))
        .to_vec();
        let sql = dml_for(
            Dialect::POSTGRES,
            &columns,
            row,
            &[
                "it's",
                "2.5",
                "[]",
                "<b/>",
                "{z}",
                "f",
                "2026-01-03 00:00:00+01",
            ],
        );
        assert_eq!(
            sql,
            [
                "update t set \"Note\" = 'it''s', r = 2.5, doc = '[]', x = '<b/>', tags = '{z}', \
                 ok = 'f', at = '2026-01-03 00:00:00+01' where id = 7 \
                 and \"Note\" = E'C:\\\\temp' and r::text = '0.1' \
                 and doc::text = '{\"a\": 1}' and x::text = '<a/>' and tags::text = '{x,y}' \
                 and ok = 't' and at = '2026-01-02 03:04:05.123456+01'",
                "delete from t where id = 7",
            ]
        );
    }

    /// SQLite: names keep their case unquoted, numbers that may be stored
    /// either way are found either way, a blob as blob or text, text
    /// without escapes; a long blob shown cut short isn't checked.
    #[test]
    fn generates_sqlite_dml() {
        let columns = vec![
            col("id", "INTEGER", true),
            col("Note", "TEXT", false),
            col("n", "TEXT", false),
            col("b", "BLOB", false),
            col("big", "BLOB", false),
            col("pythia$id", "INTEGER", true),
        ];
        let big = format!("{}… (5000 bytes)", "00".repeat(4000));
        let row = ["7", r"C:\temp", "42", "00FF", &big, "7"]
            .map(|v| Some(v.to_owned()))
            .to_vec();
        let sql = dml_for(
            Dialect::SQLITE,
            &columns,
            row,
            &["it's & co", "042", "CAFE", &big],
        );
        assert_eq!(
            sql,
            [
                "update t set Note = 'it''s ' || char(38) || ' co', n = '042', b = X'CAFE' \
                 where id in (7, '7') and Note = 'C:\\temp' and n in (42, '42') \
                 and b in (X'00FF', '00FF')",
                "delete from t where id in (7, '7')",
            ]
        );
    }

    /// A binary value held cut short is neither compared nor written, on
    /// any database (#223): its row is found by the key.
    #[test]
    fn cut_values_are_left_alone() {
        let cut = format!("{}… (5000 bytes)", "AB".repeat(4000));
        for (dialect, binary, value, new) in [
            (Dialect::MYSQL, "BLOB", cut.clone(), "CAFE"),
            (
                Dialect::SQLSERVER,
                "varbinary",
                format!("0x{cut}"),
                "0xCAFE",
            ),
            (Dialect::SQLITE, "BLOB", cut.clone(), "CAFE"),
        ] {
            let columns = vec![
                col("id", "INT", true),
                col("b", binary, false),
                col("pythia$id", "INT", true),
            ];
            let row = vec![
                Some("1".to_owned()),
                Some(value.clone()),
                Some("1".to_owned()),
            ];
            assert_eq!(same(dialect, "b", &columns[1], Some(&value)), None);
            let edit = |b: &str| dml_for(dialect, &columns, row.clone(), &[b])[0].clone();
            assert!(edit(&value).starts_with("delete"), "{dialect:?}");
            let update = edit(new);
            assert!(update.contains(&format!("b = {}", literal(dialect, &columns[1], Some(new)))));
            assert!(!update.contains(" bytes)"), "{update}");
            // Pasted into a new row, it is left out too.
            let changes = Changes {
                inserts: vec![vec![Some("2".into()), Some(value.clone()), None]],
                ..Changes::default()
            };
            let edit = Editable {
                table: "t".into(),
                source: "select id, b from t".into(),
                key: RowKey::Columns(vec!["id".into()]),
            };
            let rows = vec![row.clone()];
            let inserts = dml(dialect, &edit, &columns, &rows, &changes);
            assert!(!inserts[0].sql.contains(" bytes)"), "{}", inserts[0].sql);
            // Left out, and counted for the window to say so.
            assert_eq!(cut_changes(&edit, &columns, &changes), 1);
            let changes = Changes {
                updates: BTreeMap::from([(0, BTreeMap::from([(1, Some(value.clone()))]))]),
                inserts: vec![vec![Some("2".into()), Some(new.into()), None]],
                ..Changes::default()
            };
            assert_eq!(cut_changes(&edit, &columns, &changes), 1);
            assert!(
                dml(dialect, &edit, &columns, &rows, &changes)
                    .iter()
                    .all(|d| d.sql.starts_with("insert"))
            );
            // From a file, it is written whole.
            let mut loaded = changes.clone();
            loaded
                .loaded
                .insert((RowRef::Existing(0), 1), Loaded::Bytes(vec![1, 2].into()));
            assert_eq!(cut_changes(&edit, &columns, &loaded), 0);
        }
    }

    #[test]
    fn generates_sqlserver_dml() {
        let columns = vec![
            col("id", "int", true),
            col("name", "nvarchar", false),
            col("code", "varchar", false),
            col("r", "real", true),
            col("f", "float", true),
            col("memo", "ntext", false),
            col("x", "xml", false),
            col("v", "sql_variant", false),
            col("img", "image", false),
            col("shape", "udt", false),
            col("at", "datetime", false),
            col("pythia$id", "int", true),
        ];
        let row = [
            "7",
            "grüße",
            "A-1",
            "0.1",
            "1e300",
            "memo",
            "<a/>",
            "5",
            "0x00FF",
            "0x0101",
            "2026-01-02 03:04:05.003",
            "7",
        ]
        .map(|v| Some(v.to_owned()))
        .to_vec();
        let sql = dml_for(
            Dialect::SQLSERVER,
            &columns,
            row,
            &[
                "née",
                "B-2",
                "2.5",
                "3",
                "new",
                "<b/>",
                "6",
                "0x01",
                "0x02",
                "2026-01-03 00:00:00.000",
            ],
        );
        assert_eq!(
            sql,
            [
                "update t set name = N'née', code = 'B-2', r = 2.5, f = 3, memo = N'new', \
                 x = N'<b/>', v = N'6', img = 0x01, shape = 0x02, at = '2026-01-03T00:00:00.000' \
                 where id = 7 and name = N'grüße' and code = 'A-1' and r = cast(0.1 as real) \
                 and f = 1e300 and cast(memo as nvarchar(max)) = N'memo' \
                 and cast(x as nvarchar(max)) = N'<a/>' \
                 and cast(img as varbinary(max)) = 0x00FF \
                 and cast(shape as varbinary(max)) = 0x0101 \
                 and at = '2026-01-02T03:04:05.003'",
                "delete from t where id = 7",
            ]
        );
    }
}
