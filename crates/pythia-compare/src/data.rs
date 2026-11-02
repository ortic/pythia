//! Data compare: rows of a table (or query) on two connections, matched by
//! key columns.
//!
//! Both sides are read in key order in chunks (keyset pagination), and a
//! merge-join consumes them as they arrive, so memory stays bounded by the
//! chunk size no matter how many rows the tables have. Each chunk is a
//! separate database round trip, which lets a UI run both sides on their own
//! sessions and show progress.

use std::cmp::Ordering;
use std::collections::VecDeque;

use pythia_db::oracle::Connection;
use pythia_db::{Cell, Column};

use crate::Error;
use crate::table::ident;

/// What to read on one side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataSource {
    Table {
        schema: String,
        table: String,
    },
    /// Any query; its columns are compared like a table's.
    Query(String),
}

impl DataSource {
    fn base_sql(&self) -> String {
        match self {
            DataSource::Table { schema, table } => {
                format!("select * from {}.{}", ident(schema), ident(table))
            }
            DataSource::Query(q) => q.trim().trim_end_matches(';').to_owned(),
        }
    }

    /// Name used in the sync script.
    fn script_name(&self) -> String {
        match self {
            DataSource::Table { schema, table } => format!("{}.{}", ident(schema), ident(table)),
            DataSource::Query(_) => "target_table".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataOptions {
    /// Key columns (upper case). Empty: use the source table's primary key.
    pub key_columns: Vec<String>,
    /// Optional filter applied on both sides (`WHERE` without the keyword).
    pub filter: String,
    pub chunk_size: usize,
    /// Rows kept per difference category for display.
    pub sample_limit: usize,
    /// Maximum number of statements in the sync script.
    pub script_limit: usize,
    /// Emit DELETE statements for rows that only exist in the target.
    pub include_deletes: bool,
}

impl Default for DataOptions {
    fn default() -> Self {
        Self {
            key_columns: Vec::new(),
            filter: String::new(),
            chunk_size: 5000,
            sample_limit: 500,
            script_limit: 100_000,
            include_deletes: true,
        }
    }
}

/// One fetched chunk of rows, in key order.
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    /// No more rows after this chunk.
    pub done: bool,
}

/// Sets NLS parameters so ordering and implicit conversions are
/// deterministic: binary string order (matches Rust's byte order) and the
/// date formats the driver produces.
pub fn prepare_session(conn: &Connection) -> Result<(), Error> {
    for sql in [
        "alter session set nls_sort = binary",
        "alter session set nls_comp = binary",
        "alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS'",
        "alter session set nls_timestamp_format = 'YYYY-MM-DD HH24:MI:SS.FF'",
        "alter session set nls_numeric_characters = '.,'",
    ] {
        conn.execute(sql, &[])?;
    }
    Ok(())
}

/// Primary key columns of a table, in key order.
pub fn primary_key(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>, Error> {
    let rows = conn.query_as::<String>(
        "select c.column_name from all_constraints k \
           join all_cons_columns c on c.owner = k.owner and c.constraint_name = k.constraint_name \
          where k.owner = :1 and k.table_name = :2 and k.constraint_type = 'P' order by c.position",
        &[&schema, &table],
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Fetches the next chunk after `after` (the key of the last row read).
pub fn fetch_chunk(
    conn: &Connection,
    source: &DataSource,
    keys: &[String],
    filter: &str,
    after: Option<&[Cell]>,
    limit: usize,
) -> Result<Chunk, Error> {
    let key_list = keys.iter().map(|k| ident(k)).collect::<Vec<_>>().join(", ");
    let mut conditions: Vec<String> = keys
        .iter()
        .map(|k| format!("{} is not null", ident(k)))
        .collect();
    // On lines of their own: a `--` comment at the filter's or the
    // query's end would take what follows with it.
    if !filter.trim().is_empty() {
        conditions.push(format!("(\n{}\n)", filter.trim()));
    }
    let mut binds: Vec<String> = Vec::new();
    if let Some(after) = after {
        // (k1, k2) > (:1, :2)  ⇔  k1 > :1 or (k1 = :1 and k2 > :2)
        let mut ors = Vec::new();
        for i in 0..keys.len() {
            let mut ands = Vec::new();
            for (j, key) in keys.iter().enumerate().take(i + 1) {
                binds.push(after.get(j).cloned().flatten().unwrap_or_default());
                let op = if j == i { ">" } else { "=" };
                ands.push(format!("{} {op} :{}", ident(key), binds.len()));
            }
            ors.push(format!("({})", ands.join(" and ")));
        }
        conditions.push(format!("({})", ors.join(" or ")));
    }
    let sql = pythia_db::compat::first_rows(
        &format!(
            "select * from (\n{}\n) where {} order by {key_list}",
            source.base_sql(),
            conditions.join(" and "),
        ),
        limit.max(1),
    );
    let params: Vec<&dyn pythia_db::oracle::sql_type::ToSql> = binds
        .iter()
        .map(|b| b as &dyn pythia_db::oracle::sql_type::ToSql)
        .collect();
    let rows = conn.query(&sql, &params)?;
    let columns: Vec<Column> = rows
        .column_info()
        .iter()
        .map(|c| Column {
            name: c.name().to_owned(),
            type_name: c.oracle_type().to_string(),
            numeric: is_numeric(c.oracle_type()),
        })
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        out.push(row.sql_values().iter().map(cell).collect());
    }
    let done = out.len() < limit.max(1);
    Ok(Chunk {
        columns,
        rows: out,
        done,
    })
}

fn is_numeric(t: &pythia_db::oracle::sql_type::OracleType) -> bool {
    use pythia_db::oracle::sql_type::OracleType as T;
    matches!(
        t,
        T::Number(..) | T::Float(_) | T::BinaryFloat | T::BinaryDouble | T::Int64 | T::UInt64
    )
}

fn cell(value: &pythia_db::oracle::SqlValue) -> Cell {
    match value.is_null() {
        Ok(true) => None,
        Ok(false) => Some(value.get::<String>().unwrap_or_else(|_| value.to_string())),
        Err(e) => Some(format!("<{e}>")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSide {
    Source,
    Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedRow {
    pub key: Vec<Cell>,
    pub source: Vec<Cell>,
    pub target: Vec<Cell>,
    /// Indices (into the compared columns) that differ.
    pub changed: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct DataDiff {
    /// Columns present on both sides, in source order; the samples use them.
    pub columns: Vec<Column>,
    pub keys: Vec<String>,
    pub equal: u64,
    pub only_in_source: u64,
    pub only_in_target: u64,
    pub changed: u64,
    pub source_rows: u64,
    pub target_rows: u64,
    pub sample_only_in_source: Vec<Vec<Cell>>,
    pub sample_only_in_target: Vec<Vec<Cell>>,
    pub sample_changed: Vec<ChangedRow>,
    /// INSERT/UPDATE/DELETE statements (without terminators).
    pub statements: Vec<String>,
    pub script_truncated: bool,
    /// Column mismatches between the two sides.
    pub notes: Vec<String>,
}

impl DataDiff {
    pub fn is_equal(&self) -> bool {
        self.only_in_source == 0 && self.only_in_target == 0 && self.changed == 0
    }

    pub fn script(&self, title: &str) -> String {
        let mut out = format!("-- {title}\n");
        if self.statements.is_empty() {
            out.push_str("-- Nothing to synchronize.\n");
            return out;
        }
        if self.script_truncated {
            out.push_str("-- WARNING: script truncated; re-run the compare after applying it.\n");
        }
        for s in &self.statements {
            out.push_str(s);
            out.push_str(";\n");
        }
        out.push_str("-- Review, then commit.\n");
        out
    }
}

/// Incremental merge-join of two key-ordered row streams.
pub struct Merger {
    keys_source: Vec<usize>,
    keys_target: Vec<usize>,
    numeric_keys: Vec<bool>,
    /// (source index, target index) of compared columns.
    pairs: Vec<(usize, usize)>,
    buffers: [VecDeque<Vec<Cell>>; 2],
    done: [bool; 2],
    target_name: String,
    options: DataOptions,
    pub diff: DataDiff,
}

/// What the merger needs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    pub source: bool,
    pub target: bool,
}

impl Need {
    pub fn finished(self) -> bool {
        !self.source && !self.target
    }
}

impl Merger {
    /// Sets up the join from both sides' column lists.
    pub fn new(
        source_columns: &[Column],
        target_columns: &[Column],
        keys: &[String],
        target_name: String,
        options: DataOptions,
    ) -> Result<Self, Error> {
        let index = |cols: &[Column], name: &str| {
            cols.iter().position(|c| c.name.eq_ignore_ascii_case(name))
        };
        let mut keys_source = Vec::new();
        let mut keys_target = Vec::new();
        for k in keys {
            match (index(source_columns, k), index(target_columns, k)) {
                (Some(s), Some(t)) => {
                    keys_source.push(s);
                    keys_target.push(t);
                }
                _ => {
                    return Err(Error::Compare(format!(
                        "key column {k} is missing on one side"
                    )));
                }
            }
        }
        let mut diff = DataDiff {
            keys: keys.to_vec(),
            ..DataDiff::default()
        };
        let mut pairs = Vec::new();
        for (s, col) in source_columns.iter().enumerate() {
            match index(target_columns, &col.name) {
                Some(t) => {
                    pairs.push((s, t));
                    diff.columns.push(col.clone());
                }
                None => diff.notes.push(format!(
                    "column {} only exists in the source (not compared)",
                    col.name
                )),
            }
        }
        for col in target_columns {
            if index(source_columns, &col.name).is_none() {
                diff.notes.push(format!(
                    "column {} only exists in the target (not compared)",
                    col.name
                ));
            }
        }
        let numeric_keys = keys_source
            .iter()
            .map(|&i| source_columns[i].numeric)
            .collect();
        Ok(Self {
            keys_source,
            keys_target,
            numeric_keys,
            pairs,
            buffers: [VecDeque::new(), VecDeque::new()],
            done: [false, false],
            target_name,
            options,
            diff,
        })
    }

    pub fn push(&mut self, side: DataSide, rows: Vec<Vec<Cell>>, done: bool) {
        let i = side as usize;
        match side {
            DataSide::Source => self.diff.source_rows += rows.len() as u64,
            DataSide::Target => self.diff.target_rows += rows.len() as u64,
        }
        self.buffers[i].extend(rows);
        self.done[i] = done;
    }

    /// Joins as far as the buffered rows allow; returns which side needs more.
    pub fn advance(&mut self) -> Need {
        loop {
            let s_empty = self.buffers[0].is_empty();
            let t_empty = self.buffers[1].is_empty();
            let need = Need {
                source: s_empty && !self.done[0],
                target: t_empty && !self.done[1],
            };
            if need.source || need.target {
                return need;
            }
            match (s_empty, t_empty) {
                (true, true) => {
                    return Need {
                        source: false,
                        target: false,
                    };
                }
                (false, true) => {
                    let row = self.buffers[0].pop_front().expect("non-empty");
                    self.only_in_source(row);
                }
                (true, false) => {
                    let row = self.buffers[1].pop_front().expect("non-empty");
                    self.only_in_target(row);
                }
                (false, false) => {
                    let ord = self.compare_keys(&self.buffers[0][0], &self.buffers[1][0]);
                    match ord {
                        Ordering::Less => {
                            let row = self.buffers[0].pop_front().expect("non-empty");
                            self.only_in_source(row);
                        }
                        Ordering::Greater => {
                            let row = self.buffers[1].pop_front().expect("non-empty");
                            self.only_in_target(row);
                        }
                        Ordering::Equal => {
                            let s = self.buffers[0].pop_front().expect("non-empty");
                            let t = self.buffers[1].pop_front().expect("non-empty");
                            self.matched(s, t);
                        }
                    }
                }
            }
        }
    }

    /// The key of the last row a side delivered, for keyset pagination.
    pub fn key_of(&self, side: DataSide, row: &[Cell]) -> Vec<Cell> {
        let idx = match side {
            DataSide::Source => &self.keys_source,
            DataSide::Target => &self.keys_target,
        };
        idx.iter().map(|&i| row.get(i).cloned().flatten()).collect()
    }

    fn compare_keys(&self, s: &[Cell], t: &[Cell]) -> Ordering {
        for (k, (&si, &ti)) in self.keys_source.iter().zip(&self.keys_target).enumerate() {
            let a = s.get(si).and_then(|c| c.as_deref()).unwrap_or("");
            let b = t.get(ti).and_then(|c| c.as_deref()).unwrap_or("");
            let ord = if self.numeric_keys[k] {
                cmp_decimal(a, b)
            } else {
                a.as_bytes().cmp(b.as_bytes())
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }

    fn project_source(&self, row: &[Cell]) -> Vec<Cell> {
        self.pairs
            .iter()
            .map(|&(s, _)| row.get(s).cloned().flatten())
            .collect()
    }

    fn project_target(&self, row: &[Cell]) -> Vec<Cell> {
        self.pairs
            .iter()
            .map(|&(_, t)| row.get(t).cloned().flatten())
            .collect()
    }

    fn statement(&mut self, sql: String) {
        if self.diff.statements.len() < self.options.script_limit {
            self.diff.statements.push(sql);
        } else {
            self.diff.script_truncated = true;
        }
    }

    fn where_key(&self, row: &[Cell], keys: &[usize], cols: &[Column]) -> String {
        keys.iter()
            .map(|&i| {
                let col = &cols[i];
                format!(
                    "{} = {}",
                    ident(&col.name),
                    pythia_data::sql_literal(col, row.get(i).and_then(|c| c.as_deref()))
                )
            })
            .collect::<Vec<_>>()
            .join(" and ")
    }

    fn only_in_source(&mut self, row: Vec<Cell>) {
        self.diff.only_in_source += 1;
        let values = self.project_source(&row);
        let names: Vec<String> = self.diff.columns.iter().map(|c| ident(&c.name)).collect();
        let literals: Vec<String> = self
            .diff
            .columns
            .iter()
            .zip(&values)
            .map(|(c, v)| pythia_data::sql_literal(c, v.as_deref()))
            .collect();
        let sql = format!(
            "insert into {} ({}) values ({})",
            self.target_name,
            names.join(", "),
            literals.join(", ")
        );
        self.statement(sql);
        if self.diff.sample_only_in_source.len() < self.options.sample_limit {
            self.diff.sample_only_in_source.push(values);
        }
    }

    fn only_in_target(&mut self, row: Vec<Cell>) {
        self.diff.only_in_target += 1;
        let values = self.project_target(&row);
        if self.options.include_deletes {
            let keys: Vec<usize> = self.key_positions();
            let sql = format!(
                "delete from {} where {}",
                self.target_name,
                self.where_key(&values, &keys, &self.diff.columns)
            );
            self.statement(sql);
        }
        if self.diff.sample_only_in_target.len() < self.options.sample_limit {
            self.diff.sample_only_in_target.push(values);
        }
    }

    /// Positions of the key columns within the compared columns.
    fn key_positions(&self) -> Vec<usize> {
        self.diff
            .keys
            .iter()
            .filter_map(|k| {
                self.diff
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(k))
            })
            .collect()
    }

    fn matched(&mut self, s: Vec<Cell>, t: Vec<Cell>) {
        let source = self.project_source(&s);
        let target = self.project_target(&t);
        let changed: Vec<usize> = (0..source.len())
            .filter(|&i| !same_value(&self.diff.columns[i], &source[i], &target[i]))
            .collect();
        if changed.is_empty() {
            self.diff.equal += 1;
            return;
        }
        self.diff.changed += 1;
        let sets: Vec<String> = changed
            .iter()
            .map(|&i| {
                let col = &self.diff.columns[i];
                format!(
                    "{} = {}",
                    ident(&col.name),
                    pythia_data::sql_literal(col, source[i].as_deref())
                )
            })
            .collect();
        let keys = self.key_positions();
        let sql = format!(
            "update {} set {} where {}",
            self.target_name,
            sets.join(", "),
            self.where_key(&source, &keys, &self.diff.columns)
        );
        self.statement(sql);
        if self.diff.sample_changed.len() < self.options.sample_limit {
            let key = keys.iter().map(|&i| source[i].clone()).collect();
            self.diff.sample_changed.push(ChangedRow {
                key,
                source,
                target,
                changed,
            });
        }
    }
}

fn same_value(col: &Column, a: &Cell, b: &Cell) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) if col.numeric => cmp_decimal(x, y) == Ordering::Equal,
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Compares decimal numbers given as text exactly (no float rounding).
pub fn cmp_decimal(a: &str, b: &str) -> Ordering {
    fn parts(s: &str) -> (bool, String, String) {
        let s = s.trim();
        let (neg, s) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (int, frac) = s.split_once('.').unwrap_or((s, ""));
        let int = int.trim_start_matches('0').to_owned();
        let frac = frac.trim_end_matches('0').to_owned();
        let zero = int.is_empty() && frac.is_empty();
        (neg && !zero, int, frac)
    }
    if a.contains(['e', 'E']) || b.contains(['e', 'E']) {
        let (x, y) = (
            a.trim().parse::<f64>().unwrap_or(0.0),
            b.trim().parse::<f64>().unwrap_or(0.0),
        );
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    let (na, ia, fa) = parts(a);
    let (nb, ib, fb) = parts(b);
    let magnitude = ia
        .len()
        .cmp(&ib.len())
        .then_with(|| ia.cmp(&ib))
        .then_with(|| fa.cmp(&fb));
    match (na, nb) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => magnitude,
        (true, true) => magnitude.reverse(),
    }
}

/// Runs the merge over two in-memory or streaming row iterators (both
/// already in key order), feeding chunks like the database path does.
pub fn diff_iters(
    source_columns: &[Column],
    target_columns: &[Column],
    mut source: impl Iterator<Item = Vec<Cell>>,
    mut target: impl Iterator<Item = Vec<Cell>>,
    keys: &[String],
    target_name: &str,
    options: DataOptions,
) -> Result<DataDiff, Error> {
    let chunk = options.chunk_size.max(1);
    let mut merger = Merger::new(
        source_columns,
        target_columns,
        keys,
        target_name.to_owned(),
        options,
    )?;
    loop {
        let need = merger.advance();
        if need.finished() {
            return Ok(merger.diff);
        }
        if need.source {
            let rows: Vec<_> = source.by_ref().take(chunk).collect();
            let done = rows.len() < chunk;
            merger.push(DataSide::Source, rows, done);
        }
        if need.target {
            let rows: Vec<_> = target.by_ref().take(chunk).collect();
            let done = rows.len() < chunk;
            merger.push(DataSide::Target, rows, done);
        }
    }
}

/// Blocking compare over two connections (both on the calling thread).
pub fn compare(
    source_conn: &Connection,
    source: &DataSource,
    target_conn: &Connection,
    target: &DataSource,
    options: DataOptions,
    mut progress: impl FnMut(u64, u64),
) -> Result<DataDiff, Error> {
    prepare_session(source_conn)?;
    prepare_session(target_conn)?;
    let keys = resolve_keys(source_conn, source, &options)?;
    let limit = options.chunk_size.max(1);
    let first_s = fetch_chunk(source_conn, source, &keys, &options.filter, None, limit)?;
    let first_t = fetch_chunk(target_conn, target, &keys, &options.filter, None, limit)?;
    let mut merger = Merger::new(
        &first_s.columns,
        &first_t.columns,
        &keys,
        target.script_name(),
        options.clone(),
    )?;
    let mut last = [
        first_s
            .rows
            .last()
            .map(|r| merger.key_of(DataSide::Source, r)),
        first_t
            .rows
            .last()
            .map(|r| merger.key_of(DataSide::Target, r)),
    ];
    merger.push(DataSide::Source, first_s.rows, first_s.done);
    merger.push(DataSide::Target, first_t.rows, first_t.done);
    loop {
        let need = merger.advance();
        progress(merger.diff.source_rows, merger.diff.target_rows);
        if need.finished() {
            return Ok(merger.diff);
        }
        for (side, conn, src, wanted) in [
            (DataSide::Source, source_conn, source, need.source),
            (DataSide::Target, target_conn, target, need.target),
        ] {
            if !wanted {
                continue;
            }
            let chunk = fetch_chunk(
                conn,
                src,
                &keys,
                &options.filter,
                last[side as usize].as_deref(),
                limit,
            )?;
            if let Some(row) = chunk.rows.last() {
                last[side as usize] = Some(merger.key_of(side, row));
            }
            merger.push(side, chunk.rows, chunk.done);
        }
    }
}

/// Key columns from the options or the source table's primary key.
pub fn resolve_keys(
    conn: &Connection,
    source: &DataSource,
    options: &DataOptions,
) -> Result<Vec<String>, Error> {
    if !options.key_columns.is_empty() {
        return Ok(options
            .key_columns
            .iter()
            .map(|k| k.trim().to_uppercase())
            .filter(|k| !k.is_empty())
            .collect());
    }
    match source {
        DataSource::Table { schema, table } => {
            let pk = primary_key(conn, schema, table)?;
            if pk.is_empty() {
                Err(Error::Compare(format!(
                    "{table} has no primary key: choose key columns"
                )))
            } else {
                Ok(pk)
            }
        }
        DataSource::Query(_) => Err(Error::Compare("choose key columns for a query".into())),
    }
}

impl DataSource {
    pub fn target_name(&self) -> String {
        self.script_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[(&str, &str, bool)]) -> Vec<Column> {
        names
            .iter()
            .map(|(n, t, num)| Column {
                name: (*n).into(),
                type_name: (*t).into(),
                numeric: *num,
            })
            .collect()
    }

    fn row(values: &[Option<&str>]) -> Vec<Cell> {
        values.iter().map(|v| v.map(str::to_owned)).collect()
    }

    #[test]
    fn decimal_order() {
        assert_eq!(cmp_decimal("9", "10"), Ordering::Less);
        assert_eq!(cmp_decimal("-9", "-10"), Ordering::Greater);
        assert_eq!(cmp_decimal("1.50", "1.5"), Ordering::Equal);
        assert_eq!(cmp_decimal("0.1", "-0"), Ordering::Greater);
        assert_eq!(
            cmp_decimal("123456789012345678901", "123456789012345678902"),
            Ordering::Less
        );
    }

    #[test]
    fn merge_join_finds_all_differences() {
        let source_cols = cols(&[
            ("ID", "NUMBER", true),
            ("NAME", "VARCHAR2(10)", false),
            ("ONLY_S", "NUMBER", true),
        ]);
        let target_cols = cols(&[("NAME", "VARCHAR2(10)", false), ("ID", "NUMBER", true)]);
        let source = vec![
            row(&[Some("1"), Some("a"), Some("0")]),
            row(&[Some("2"), Some("b"), None]),
            row(&[Some("4"), Some("d"), None]),
            row(&[Some("10"), Some("O'Neil"), None]),
        ];
        let target = vec![
            row(&[Some("a"), Some("1")]),
            row(&[Some("x"), Some("2")]),
            row(&[Some("c"), Some("3")]),
            row(&[Some("j"), Some("10")]),
            row(&[Some("z"), Some("11")]),
        ];
        let options = DataOptions {
            chunk_size: 2,
            ..DataOptions::default()
        };
        let d = diff_iters(
            &source_cols,
            &target_cols,
            source.into_iter(),
            target.into_iter(),
            &["ID".into()],
            "t",
            options,
        )
        .unwrap();
        assert_eq!(
            (d.equal, d.changed, d.only_in_source, d.only_in_target),
            (1, 2, 1, 2)
        );
        assert_eq!((d.source_rows, d.target_rows), (4, 5));
        assert_eq!(
            d.notes,
            ["column ONLY_S only exists in the source (not compared)"]
        );
        assert_eq!(
            d.statements,
            [
                "update t set name = 'b' where id = 2",
                "delete from t where id = 3",
                "insert into t (id, name) values (4, 'd')",
                "update t set name = 'O''Neil' where id = 10",
                "delete from t where id = 11",
            ]
        );
        assert_eq!(d.sample_changed[0].changed, [1]);
        assert!(!d.is_equal());
    }

    #[test]
    fn composite_keys_and_limits() {
        let c = cols(&[
            ("A", "VARCHAR2(5)", false),
            ("B", "NUMBER", true),
            ("V", "NUMBER", true),
        ]);
        let source: Vec<Vec<Cell>> = (0..50)
            .map(|i| row(&[Some("k"), Some(&i.to_string()), Some("1")]))
            .collect();
        let target: Vec<Vec<Cell>> = Vec::new();
        let options = DataOptions {
            chunk_size: 7,
            sample_limit: 3,
            script_limit: 10,
            include_deletes: false,
            ..DataOptions::default()
        };
        let d = diff_iters(
            &c,
            &c,
            source.into_iter(),
            target.into_iter(),
            &["A".into(), "B".into()],
            "t",
            options,
        )
        .unwrap();
        assert_eq!(d.only_in_source, 50);
        assert_eq!(d.sample_only_in_source.len(), 3);
        assert_eq!(d.statements.len(), 10);
        assert!(d.script_truncated);
        assert!(d.script("x").contains("truncated"));
    }

    #[test]
    fn missing_key_column() {
        let c = cols(&[("A", "NUMBER", true)]);
        assert!(Merger::new(&c, &c, &["B".into()], "t".into(), DataOptions::default()).is_err());
    }

    #[test]
    fn equal_data() {
        let c = cols(&[("ID", "NUMBER", true)]);
        let rows: Vec<Vec<Cell>> = (0..10).map(|i| row(&[Some(&i.to_string())])).collect();
        let d = diff_iters(
            &c,
            &c,
            rows.clone().into_iter(),
            rows.into_iter(),
            &["ID".into()],
            "t",
            DataOptions::default(),
        )
        .unwrap();
        assert!(d.is_equal());
        assert_eq!(d.equal, 10);
        assert!(d.script("x").contains("Nothing to synchronize"));
    }
}
