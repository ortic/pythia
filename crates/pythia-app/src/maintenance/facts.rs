//! The state of an object, as evidence for what it needs and as what the
//! statements are made of: statistics and their age, unusable indexes, dead
//! rows, reclaimable space, fragmentation.

use pythia_db::oracle::Connection;
use pythia_db::{Cell, Client, DbError, DbKind, ObjectType};
use pythia_monitor::ui::style::{bytes, duration};

use crate::documents::ObjectRef;

#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// The state shown.
    pub state: Vec<Fact>,
    /// The edition can rebuild and move online (Oracle's Enterprise
    /// Edition, Free and XE; SQL Server's Enterprise and Developer).
    pub online: bool,
    /// The server's major and minor release.
    pub release: (i32, i32),
    /// Oracle: gathering and deleting need force.
    pub stats_locked: bool,
    pub partitioned: bool,
    pub temporary: bool,
    /// An index-organized table.
    pub iot: bool,
    pub row_movement: bool,
    /// Oracle: the index's type (`NORMAL`, `BITMAP`, `IOT - TOP`, …).
    pub index_type: String,
    /// Oracle: what rebuilding the table's indexes (or the index) takes.
    pub segments: Vec<IndexSegment>,
    /// SQL Server: indexes of the table.
    pub indexes: usize,
}

/// A line of the state: what, its value, and whether it wants attention.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub label: String,
    pub value: String,
    pub warn: bool,
}

/// An index, or one of its partitions or subpartitions: what Oracle
/// rebuilds at a time.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSegment {
    pub owner: String,
    pub index: String,
    pub part: Part,
    pub unusable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Whole,
    Partition(String),
    Subpartition(String),
}

impl Facts {
    fn add(&mut self, label: impl Into<String>, value: impl Into<String>, warn: bool) {
        self.state.push(Fact {
            label: label.into(),
            value: value.into(),
            warn,
        });
    }
}

/// `1,234,567`.
pub fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// How long ago, from seconds; none is "never".
fn ago(secs: Option<f64>) -> String {
    match secs {
        Some(s) => format!("{} ago", duration(s)),
        None => "never".into(),
    }
}

// ----- Oracle ---------------------------------------------------------------

/// An Oracle table, materialized view or index.
pub fn oracle(conn: &Connection, object: &ObjectRef) -> Result<Facts, DbError> {
    let mut f = Facts::default();
    let (version, banner) = conn.server_version()?;
    f.release = (version.major(), version.minor());
    // Online operations are an Enterprise Edition feature (Free and XE
    // have them too); unknown editions get the benefit of the doubt.
    f.online = !banner.contains("Standard Edition");
    // What changed since the last flush counts for staleness. Flushing
    // takes ANALYZE ANY: without it, staleness lags by a few minutes.
    let _ = conn.execute("begin dbms_stats.flush_database_monitoring_info; end;", &[]);
    if object.object_type == ObjectType::Index {
        oracle_index(conn, object, &mut f)?;
    } else {
        oracle_table(conn, object, &mut f)?;
    }
    if let Some(size) = oracle_size(conn, object) {
        f.add("Size", bytes(size), false);
    }
    if !f.online {
        f.add("Online operations", "not in Standard Edition", false);
    }
    Ok(f)
}

fn oracle_table(conn: &Connection, object: &ObjectRef, f: &mut Facts) -> Result<(), DbError> {
    let binds: [(&str, &dyn pythia_db::oracle::sql_type::ToSql); 2] =
        [("owner", &object.owner), ("name", &object.name)];
    let row = conn
        .query_row_named(
            "select t.num_rows, t.blocks, (sysdate - t.last_analyzed) * 86400,
                    s.stale_stats, s.stattype_locked, t.partitioned, t.row_movement,
                    t.temporary, t.iot_type
               from all_tables t
               left join all_tab_statistics s
                 on s.owner = t.owner and s.table_name = t.table_name
                and s.object_type = 'TABLE'
              where t.owner = :owner and t.table_name = :name",
            &binds,
        )
        .map_err(|e| not_found(e, object))?;
    let rows: Option<i64> = row.get(0)?;
    let analyzed: Option<f64> = row.get(2)?;
    let stale: Option<String> = row.get(3)?;
    let locked: Option<String> = row.get(4)?;
    f.partitioned = row.get::<_, Option<String>>(5)?.as_deref() == Some("YES");
    f.row_movement = row.get::<_, Option<String>>(6)?.as_deref() == Some("ENABLED");
    f.temporary = row.get::<_, Option<String>>(7)?.as_deref() == Some("Y");
    f.iot = row.get::<_, Option<String>>(8)?.is_some();
    stats_facts(f, rows, analyzed, stale, locked);
    if f.partitioned {
        f.add("Partitioned", "yes", false);
    }
    if f.temporary {
        f.add("Temporary", "yes", false);
    }
    if f.iot {
        f.add("Organization", "index", false);
    }
    f.add(
        "Row movement",
        if f.row_movement {
            "enabled"
        } else {
            "disabled"
        },
        false,
    );
    f.segments = oracle_segments(
        conn,
        "i.table_owner = :owner and i.table_name = :name",
        &binds,
    )?;
    let indexes: std::collections::BTreeSet<(&str, &str)> = f
        .segments
        .iter()
        .map(|s| (s.owner.as_str(), s.index.as_str()))
        .collect();
    let unusable = f.segments.iter().filter(|s| s.unusable).count();
    let value = match unusable {
        0 => grouped(indexes.len() as i64),
        n => format!(
            "{}, {} unusable",
            grouped(indexes.len() as i64),
            unusable_text(n, &f.segments)
        ),
    };
    f.add("Indexes", value, unusable > 0);
    Ok(())
}

fn oracle_index(conn: &Connection, object: &ObjectRef, f: &mut Facts) -> Result<(), DbError> {
    let binds: [(&str, &dyn pythia_db::oracle::sql_type::ToSql); 2] =
        [("owner", &object.owner), ("name", &object.name)];
    let row = conn
        .query_row_named(
            "select i.index_type, i.status, i.partitioned, i.num_rows, i.leaf_blocks, i.blevel,
                    (sysdate - i.last_analyzed) * 86400, s.stale_stats, s.stattype_locked,
                    i.table_owner, i.table_name
               from all_indexes i
               left join all_ind_statistics s
                 on s.owner = i.owner and s.index_name = i.index_name
                and s.object_type = 'INDEX'
              where i.owner = :owner and i.index_name = :name",
            &binds,
        )
        .map_err(|e| not_found(e, object))?;
    f.index_type = row.get::<_, Option<String>>(0)?.unwrap_or_default();
    let status: Option<String> = row.get(1)?;
    f.partitioned = row.get::<_, Option<String>>(2)?.as_deref() == Some("YES");
    let rows: Option<i64> = row.get(3)?;
    let leaf_blocks: Option<i64> = row.get(4)?;
    let levels: Option<i64> = row.get(5)?;
    let analyzed: Option<f64> = row.get(6)?;
    let stale: Option<String> = row.get(7)?;
    let locked: Option<String> = row.get(8)?;
    let table_owner: Option<String> = row.get(9)?;
    let table: Option<String> = row.get(10)?;
    if let (Some(owner), Some(table)) = (table_owner, table) {
        let table = ObjectRef {
            owner,
            name: table,
            object_type: ObjectType::Table,
        };
        let shown = crate::browser::shown_qualified(pythia_db::Dialect::ORACLE, &table);
        f.add("Table", shown, false);
    }
    f.add("Type", f.index_type.to_lowercase(), false);
    f.segments = oracle_segments(conn, "i.owner = :owner and i.index_name = :name", &binds)?;
    let unusable = f.segments.iter().filter(|s| s.unusable).count();
    let status = if f.partitioned {
        let parts = f.segments.len();
        match unusable {
            0 => format!("{} partitions, all usable", grouped(parts as i64)),
            n => format!("{n} of {} partitions unusable", grouped(parts as i64)),
        }
    } else {
        status.unwrap_or_default().to_lowercase()
    };
    f.add("Status", status, unusable > 0);
    stats_facts(f, rows, analyzed, stale, locked);
    if let Some(leaf_blocks) = leaf_blocks {
        f.add("Leaf blocks", grouped(leaf_blocks), false);
    }
    if let Some(levels) = levels {
        // BLEVEL counts the branch levels above the leaves.
        f.add("Height", grouped(levels + 1), false);
    }
    Ok(())
}

/// Rows, when statistics were gathered, staleness and locks.
fn stats_facts(
    f: &mut Facts,
    rows: Option<i64>,
    analyzed: Option<f64>,
    stale: Option<String>,
    locked: Option<String>,
) {
    if let Some(rows) = rows {
        f.add("Rows (statistics)", grouped(rows), false);
    }
    f.add("Statistics gathered", ago(analyzed), analyzed.is_none());
    if stale.as_deref() == Some("YES") {
        f.add(
            "Statistics stale",
            "yes, over 10% of the rows changed since",
            true,
        );
    }
    f.stats_locked = locked.is_some();
    if let Some(locked) = locked {
        let what = match locked.as_str() {
            "ALL" => "yes",
            "DATA" => "yes (data)",
            "CACHE" => "yes (cache)",
            other => other,
        };
        f.add("Statistics locked", what, true);
    }
}

/// "2 indexes" / "1 index and 3 partitions": what is unusable.
fn unusable_text(n: usize, segments: &[IndexSegment]) -> String {
    let parts = segments
        .iter()
        .filter(|s| s.unusable && s.part != Part::Whole)
        .count();
    let whole = n - parts;
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (whole, parts) {
        (w, 0) => plural(w, "index", "indexes"),
        (0, p) => plural(p, "partition", "partitions"),
        (w, p) => format!(
            "{} and {}",
            plural(w, "index", "indexes"),
            plural(p, "partition", "partitions")
        ),
    }
}

/// The segments of the indexes `filter` picks: non-partitioned indexes,
/// partitions, and the subpartitions of composite ones (whose partitions
/// have no segment). LOB indexes and an IOT's own index are left out:
/// they aren't rebuilt as indexes.
fn oracle_segments(
    conn: &Connection,
    filter: &str,
    binds: &[(&str, &dyn pythia_db::oracle::sql_type::ToSql)],
) -> Result<Vec<IndexSegment>, DbError> {
    let sql = format!(
        "select i.owner, i.index_name, null, null, i.status
           from all_indexes i
          where {filter} and i.partitioned = 'NO' and i.index_type not in ('LOB', 'IOT - TOP')
         union all
         select p.index_owner, p.index_name, p.partition_name, null, p.status
           from all_indexes i
           join all_ind_partitions p on p.index_owner = i.owner and p.index_name = i.index_name
          where {filter} and p.status <> 'N/A'
         union all
         select s.index_owner, s.index_name, null, s.subpartition_name, s.status
           from all_indexes i
           join all_ind_subpartitions s on s.index_owner = i.owner and s.index_name = i.index_name
          where {filter}
          order by 2, 3, 4"
    );
    let mut out = Vec::new();
    for row in conn.query_named(&sql, binds)? {
        let row = row?;
        let partition: Option<String> = row.get(2)?;
        let subpartition: Option<String> = row.get(3)?;
        let status: Option<String> = row.get(4)?;
        out.push(IndexSegment {
            owner: row.get(0)?,
            index: row.get(1)?,
            part: match (partition, subpartition) {
                (Some(p), _) => Part::Partition(p),
                (_, Some(s)) => Part::Subpartition(s),
                _ => Part::Whole,
            },
            unusable: status.as_deref() == Some("UNUSABLE"),
        });
    }
    Ok(out)
}

/// The object's segments in bytes: from DBA_SEGMENTS, or USER_SEGMENTS
/// for one's own; none when neither can be read.
fn oracle_size(conn: &Connection, object: &ObjectRef) -> Option<f64> {
    let binds: [(&str, &dyn pythia_db::oracle::sql_type::ToSql); 2] =
        [("owner", &object.owner), ("name", &object.name)];
    [
        "select sum(bytes) from dba_segments where owner = :owner and segment_name = :name",
        "select sum(bytes) from user_segments where segment_name = :name and user = :owner",
    ]
    .iter()
    .find_map(|sql| {
        conn.query_row_named(sql, &binds)
            .ok()?
            .get::<_, Option<f64>>(0)
            .ok()
    })
    .flatten()
}

fn not_found(e: pythia_db::oracle::Error, object: &ObjectRef) -> DbError {
    if e.kind() == pythia_db::oracle::ErrorKind::NoDataFound {
        DbError::new(format!(
            "{} isn't there or isn't visible to you",
            object.qualified()
        ))
    } else {
        e.into()
    }
}

// ----- MySQL, PostgreSQL, SQL Server, SQLite --------------------------------

/// A table (or PostgreSQL's materialized view) of another database.
pub fn other(client: &mut dyn Client, object: &ObjectRef) -> Result<Facts, DbError> {
    match client.dialect().kind() {
        DbKind::Postgres => postgres(client, object),
        DbKind::MySql => mysql(client, object),
        DbKind::SqlServer => sqlserver(client, object),
        DbKind::Sqlite => sqlite(client, object),
        DbKind::Oracle => Err(DbError::new("Oracle's state comes from oracle()")),
    }
}

/// A SQLite table or index: its rows (from the statistics, else counted),
/// its pages and those of its indexes, whether ANALYZE ever ran on it, and
/// the space of the whole file that VACUUM would give back.
fn sqlite(c: &mut dyn Client, object: &ObjectRef) -> Result<Facts, DbError> {
    let mut f = Facts::default();
    let d = pythia_db::Dialect::SQLITE;
    let schema = d.quote_ident(&object.owner);
    let table = if object.object_type == ObjectType::Index {
        let rows = c.query(
            &format!(
                "select tbl_name from {schema}.sqlite_schema where type = 'index' \
                  and name = ? collate nocase"
            ),
            &[&object.name],
        )?;
        text(&first_row(rows, object)?, 0).unwrap_or_default()
    } else {
        object.name.clone()
    };
    let stats = c
        .query(
            &format!(
                "select stat from {schema}.sqlite_stat1 where tbl = ? collate nocase \
                  order by idx is not null limit 1"
            ),
            &[&table],
        )
        .ok()
        .and_then(|rows| rows.into_iter().next());
    let estimate = stats
        .as_ref()
        .and_then(|row| text(row, 0))
        .and_then(|stat| stat.split_whitespace().next()?.parse::<i64>().ok());
    match estimate {
        Some(rows) => f.add("Rows (statistics)", grouped(rows), false),
        None => {
            let counted = c.query(
                &format!(
                    "select count(*) from {}",
                    d.qualified(&object.owner, &table)
                ),
                &[],
            )?;
            if let Some(rows) = counted.first().and_then(|r| int(r, 0)) {
                f.add("Rows", grouped(rows), false);
            }
        }
    }
    // dbstat sums each object's pages (its first parameter: the schema).
    let size = |c: &mut dyn Client, names: &str, name: &str| -> Option<f64> {
        let sql = format!("select sum(pgsize) from dbstat(?, 1) where name in ({names})");
        c.query(&sql, &[&object.owner, name])
            .ok()?
            .first()
            .and_then(|r| num(r, 0))
    };
    let indexes = format!(
        "select name from {schema}.sqlite_schema where type = 'index' and tbl_name = ? \
          collate nocase"
    );
    if object.object_type == ObjectType::Index {
        if let Some(bytes_used) = size(c, "?", &object.name) {
            f.add("Size", bytes(bytes_used), false);
        }
    } else {
        if let Some(data) = size(c, "?", &table) {
            f.add("Data", bytes(data), false);
        }
        if let Some(index) = size(c, &indexes, &table) {
            f.add("Indexes", bytes(index), false);
        }
        let count = c.query(&format!("select count(*) from ({indexes})"), &[&table])?;
        f.indexes = count
            .first()
            .and_then(|r| int(r, 0))
            .unwrap_or_default()
            .max(0) as usize;
    }
    f.add(
        "Statistics",
        if stats.is_some() {
            "gathered (ANALYZE)"
        } else {
            "never gathered"
        },
        stats.is_none() && f.indexes > 0,
    );
    let pages = |c: &mut dyn Client, pragma: &str| -> Option<f64> {
        c.query(&format!("pragma {schema}.{pragma}"), &[])
            .ok()?
            .first()
            .and_then(|r| num(r, 0))
    };
    if let (Some(free), Some(total), Some(page)) = (
        pages(c, "freelist_count"),
        pages(c, "page_count"),
        pages(c, "page_size"),
    ) {
        f.add(
            "Free in the file",
            format!(
                "{} of {} (VACUUM gives it back)",
                bytes(free * page),
                bytes(total * page)
            ),
            free * page >= 32.0 * 1024.0 * 1024.0 && free > 0.2 * total,
        );
    }
    Ok(f)
}

fn int(row: &[Cell], i: usize) -> Option<i64> {
    let text = row.get(i)?.as_deref()?;
    text.parse::<i64>()
        .ok()
        .or_else(|| text.parse::<f64>().ok().map(|f| f as i64))
}

fn num(row: &[Cell], i: usize) -> Option<f64> {
    row.get(i)?.as_deref()?.parse().ok()
}

fn text(row: &[Cell], i: usize) -> Option<String> {
    row.get(i)?.clone()
}

fn first_row(rows: Vec<Vec<Cell>>, object: &ObjectRef) -> Result<Vec<Cell>, DbError> {
    rows.into_iter().next().ok_or_else(|| {
        DbError::new(format!(
            "{}.{} isn't there or isn't visible to you",
            object.owner, object.name
        ))
    })
}

fn postgres(c: &mut dyn Client, object: &ObjectRef) -> Result<Facts, DbError> {
    let mut f = Facts {
        online: true,
        ..Facts::default()
    };
    let mut row = first_row(
        c.query(
            "select s.n_live_tup, s.n_dead_tup, \
                    extract(epoch from now() - greatest(s.last_vacuum, s.last_autovacuum)), \
                    s.last_autovacuum is not null \
                      and s.last_autovacuum >= coalesce(s.last_vacuum, s.last_autovacuum), \
                    extract(epoch from now() - greatest(s.last_analyze, s.last_autoanalyze)), \
                    s.last_autoanalyze is not null \
                      and s.last_autoanalyze >= coalesce(s.last_analyze, s.last_autoanalyze), \
                    s.n_mod_since_analyze, pg_total_relation_size(c.oid), age(c.relfrozenxid), \
                    current_setting('server_version_num'), \
                    current_setting('autovacuum_freeze_max_age'), \
                    (select count(*) from pg_index i where i.indrelid = c.oid), \
                    (select count(*) from pg_index i where i.indrelid = c.oid \
                        and not i.indisvalid), \
                    c.relkind = 'p' \
               from pg_class c \
               join pg_namespace n on n.oid = c.relnamespace \
               left join pg_stat_all_tables s on s.relid = c.oid \
              where n.nspname = $1 and c.relname = $2",
            &[&object.owner, &object.name],
        )?,
        object,
    )?;
    // A partitioned table holds no rows, has no size and no frozen
    // transaction ID (its age is the largest there is): its partitions'
    // are its own (#442).
    if text(&row, 13).as_deref() == Some("t") {
        let leaves = first_row(
            c.query(
                "with recursive tree (oid, kind) as ( \
                   select c.oid, c.relkind from pg_class c \
                     join pg_namespace n on n.oid = c.relnamespace \
                    where n.nspname = $1 and c.relname = $2 \
                   union all \
                   select k.oid, k.relkind from tree \
                     join pg_inherits i on i.inhparent = tree.oid \
                     join pg_class k on k.oid = i.inhrelid \
                    where tree.kind = 'p') \
                 select sum(s.n_live_tup), sum(s.n_dead_tup), \
                        sum(pg_total_relation_size(t.oid)), \
                        max(age(k.relfrozenxid)) filter (where k.relkind = 'r') \
                   from tree t \
                   join pg_class k on k.oid = t.oid \
                   left join pg_stat_all_tables s on s.relid = t.oid \
                  where t.kind <> 'p'",
                &[&object.owner, &object.name],
            )?,
            object,
        )?;
        for (from, to) in [(0, 0), (1, 1), (2, 7), (3, 8)] {
            row[to] = leaves[from].clone();
        }
    }
    let version = int(&row, 9).unwrap_or(0);
    f.release = ((version / 10000) as i32, (version % 10000 / 100) as i32);
    let live = int(&row, 0).unwrap_or(0);
    let dead = int(&row, 1).unwrap_or(0);
    f.add("Rows", grouped(live), false);
    let share = 100.0 * dead as f64 / (live + dead).max(1) as f64;
    f.add(
        "Dead rows",
        if dead == 0 {
            "0".into()
        } else {
            format!("{} ({share:.0}%)", grouped(dead))
        },
        dead >= 1000 && share > 20.0,
    );
    let by = |auto: bool, word: &str| {
        if auto {
            format!(" ({word})")
        } else {
            String::new()
        }
    };
    let vacuumed = num(&row, 2);
    f.add(
        "Last vacuum",
        format!(
            "{}{}",
            ago(vacuumed),
            by(text(&row, 3).as_deref() == Some("t"), "autovacuum")
        ),
        false,
    );
    let analyzed = num(&row, 4);
    f.add(
        "Last analyze",
        format!(
            "{}{}",
            ago(analyzed),
            by(text(&row, 5).as_deref() == Some("t"), "autoanalyze")
        ),
        analyzed.is_none() && live > 0,
    );
    if let Some(changed) = int(&row, 6) {
        // Autovacuum analyzes after 10% (autovacuum_analyze_scale_factor).
        f.add(
            "Changed since analyze",
            grouped(changed),
            live > 0 && changed > live / 10 && changed >= 1000,
        );
    }
    if let Some(size) = num(&row, 7) {
        f.add("Size", bytes(size), false);
    }
    if let Some(age) = int(&row, 8) {
        // Past autovacuum_freeze_max_age, autovacuum must freeze it.
        let limit = int(&row, 10).unwrap_or(200_000_000);
        f.add("Transaction ID age", grouped(age), age > limit);
    }
    let indexes = int(&row, 11).unwrap_or(0);
    let invalid = int(&row, 12).unwrap_or(0);
    f.indexes = indexes as usize;
    f.add(
        "Indexes",
        match invalid {
            0 => grouped(indexes),
            n => format!("{}, {n} invalid", grouped(indexes)),
        },
        invalid > 0,
    );
    Ok(f)
}

fn mysql(c: &mut dyn Client, object: &ObjectRef) -> Result<Facts, DbError> {
    let mut f = Facts::default();
    // MySQL 8 caches a table's sizes and last change for a day: after an
    // OPTIMIZE, or rows written, they were the old ones (#441). MariaDB
    // has no such cache (and no variable).
    let _ = c.execute("set session information_schema_stats_expiry = 0");
    let row = first_row(
        c.query(
            "select engine, table_rows, data_length, index_length, data_free, \
                    timestampdiff(second, update_time, now()) \
               from information_schema.tables \
              where table_schema = ? and table_name = ?",
            &[&object.owner, &object.name],
        )?,
        object,
    )?;
    if let Some(engine) = text(&row, 0) {
        f.add("Engine", engine, false);
    }
    if let Some(rows) = int(&row, 1) {
        f.add("Rows (estimate)", grouped(rows), false);
    }
    let data = num(&row, 2).unwrap_or(0.0);
    let index = num(&row, 3).unwrap_or(0.0);
    let free = num(&row, 4).unwrap_or(0.0);
    f.add("Data", bytes(data), false);
    f.add("Indexes", bytes(index), false);
    // InnoDB keeps a few free extents anyway.
    f.add(
        "Reclaimable",
        bytes(free),
        free >= 32.0 * 1024.0 * 1024.0 && free > 0.2 * (data + index),
    );
    if let Some(changed) = num(&row, 5) {
        f.add("Last change", ago(Some(changed)), false);
    }
    // InnoDB's persistent statistics, where they can be read.
    if let Ok(rows) = c.query(
        "select timestampdiff(second, last_update, now()) from mysql.innodb_table_stats \
          where database_name = ? and table_name = ?",
        &[&object.owner, &object.name],
    ) && let Some(row) = rows.first()
    {
        f.add("Statistics updated", ago(num(row, 0)), false);
    }
    Ok(f)
}

fn sqlserver(c: &mut dyn Client, object: &ObjectRef) -> Result<Facts, DbError> {
    let mut f = Facts::default();
    let name = pythia_db::Dialect::SQLSERVER.qualified(&object.owner, &object.name);
    let row = first_row(
        c.query(
            "select (select sum(p.rows) from sys.partitions p \
                      where p.object_id = o.object_id and p.index_id in (0, 1)), \
                    (select count(*) from sys.indexes i \
                      where i.object_id = o.object_id and i.type > 0), \
                    cast(serverproperty('EngineEdition') as int), \
                    cast(serverproperty('ProductMajorVersion') as int) \
               from sys.objects o where o.object_id = object_id(@P1)",
            &[&name],
        )?,
        object,
    )?;
    // Enterprise (and Developer), Azure SQL Database, Managed Instance.
    f.online = matches!(int(&row, 2), Some(3 | 5 | 8));
    f.release = (int(&row, 3).unwrap_or(0) as i32, 0);
    if let Some(rows) = int(&row, 0) {
        f.add("Rows", grouped(rows), false);
    }
    f.indexes = int(&row, 1).unwrap_or(0) as usize;
    f.add("Indexes", grouped(f.indexes as i64), false);
    match c.query(
        "select count(*), max(datediff(second, sp.last_updated, getdate())), \
                sum(case when sp.last_updated is null then 1 else 0 end), \
                max(sp.modification_counter), max(sp.rows) \
           from sys.stats s \
          cross apply sys.dm_db_stats_properties(s.object_id, s.stats_id) sp \
          where s.object_id = object_id(@P1)",
        &[&name],
    ) {
        Ok(rows) => {
            if let Some(row) = rows.first()
                && int(row, 0).unwrap_or(0) > 0
            {
                let never = int(row, 2).unwrap_or(0);
                let oldest = if never > 0 {
                    format!("{never} never updated")
                } else {
                    format!("oldest {}", ago(num(row, 1)))
                };
                f.add(
                    "Statistics",
                    format!("{}, {oldest}", grouped(int(row, 0).unwrap_or(0))),
                    never > 0,
                );
                let changed = int(row, 3).unwrap_or(0);
                let rows = int(row, 4).unwrap_or(0);
                f.add(
                    "Changed since update",
                    format!("up to {} rows", grouped(changed)),
                    rows > 0 && changed > rows / 5 && changed >= 1000,
                );
            }
        }
        Err(e) => f.add("Statistics", first_line(&e.message), false),
    }
    if f.indexes > 0 {
        // Microsoft's rule of thumb: reorganize over 5%, rebuild over
        // 30%, and leave small indexes alone.
        match c.query(
            "select i.name, ps.avg_fragmentation_in_percent, ps.page_count \
               from sys.dm_db_index_physical_stats(db_id(), object_id(@P1), null, null, \
                    'LIMITED') ps \
               join sys.indexes i on i.object_id = ps.object_id and i.index_id = ps.index_id \
              where ps.index_id > 0 and ps.alloc_unit_type_desc = 'IN_ROW_DATA' \
              order by i.name",
            &[&name],
        ) {
            Ok(rows) => {
                for row in rows {
                    let pct = num(&row, 1).unwrap_or(0.0);
                    let pages = int(&row, 2).unwrap_or(0);
                    f.add(
                        format!("Index {}", text(&row, 0).unwrap_or_default()),
                        format!("{pct:.0}% fragmented, {} pages", grouped(pages)),
                        pct > 30.0 && pages >= 1000,
                    );
                }
            }
            Err(e) => f.add("Fragmentation", first_line(&e.message), false),
        }
    }
    if !f.online {
        f.add("Online rebuilds", "Enterprise Edition only", false);
    }
    Ok(f)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MySQL's facts are the table's as it is, not as it was cached when
    /// they were first read (#441): its last change, after rows written.
    #[test]
    fn mysql_facts_are_current() {
        crate::test_db::run_on("PYTHIA_TEST_MYSQL", DbKind::MySql, |c| {
            let schema = c.query("select database()", &[]).unwrap()[0][0]
                .clone()
                .unwrap();
            c.execute("drop table if exists fd_facts").unwrap();
            c.execute("create table fd_facts (n int primary key)")
                .unwrap();
            let table = ObjectRef {
                owner: schema,
                name: "fd_facts".into(),
                object_type: ObjectType::Table,
            };
            let changed = |c: &mut dyn Client| {
                other(c, &table)
                    .unwrap()
                    .state
                    .iter()
                    .any(|f| f.label == "Last change")
            };
            // Read once, as the facts of a new table are.
            // MariaDB has no cache.
            let _ = c.execute("set session information_schema_stats_expiry = 86400");
            assert!(!changed(c));
            c.execute("insert into fd_facts values (1)").unwrap();
            // MariaDB has no cache.
            let _ = c.execute("set session information_schema_stats_expiry = 86400");
            assert!(changed(c));
            c.execute("drop table fd_facts").unwrap();
        });
    }

    /// A partitioned table's facts are its partitions' (#442): their rows
    /// and size, the oldest transaction ID of theirs, which was the largest
    /// age there is, past the wraparound limit.
    #[test]
    fn postgres_partitioned_tables_are_their_partitions() {
        crate::test_db::run_on("PYTHIA_TEST_POSTGRES", DbKind::Postgres, |c| {
            for sql in [
                "drop table if exists fd_parted",
                "create table fd_parted (n int) partition by range (n)",
                "create table fd_parted_low partition of fd_parted for values from (0) to (100) \
                   partition by range (n)",
                "create table fd_parted_low1 partition of fd_parted_low for values from (0) to (50)",
                "create table fd_parted_low2 partition of fd_parted_low \
                   for values from (50) to (100)",
                "create table fd_parted_high partition of fd_parted for values from (100) to (200)",
                "insert into fd_parted select generate_series(0, 149)",
                "analyze fd_parted",
            ] {
                c.execute(sql).unwrap();
            }
            let table = ObjectRef {
                owner: "public".into(),
                name: "fd_parted".into(),
                object_type: ObjectType::Table,
            };
            let facts = other(c, &table).unwrap();
            let fact = |label: &str| {
                facts
                    .state
                    .iter()
                    .find(|f| f.label == label)
                    .unwrap_or_else(|| panic!("{label}"))
            };
            assert_eq!(fact("Rows").value, "150");
            assert_ne!(fact("Size").value, bytes(0.0));
            let age = fact("Transaction ID age");
            assert!(!age.warn, "{}", age.value);
            c.execute("drop table fd_parted").unwrap();
        });
    }

    #[test]
    fn groups_digits() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(grouped(-12345), "-12,345");
    }

    #[test]
    fn names_what_is_unusable() {
        let seg = |part, unusable| IndexSegment {
            owner: "A".into(),
            index: "I".into(),
            part,
            unusable,
        };
        let segments = [
            seg(Part::Whole, true),
            seg(Part::Partition("P1".into()), true),
            seg(Part::Partition("P2".into()), true),
            seg(Part::Whole, false),
        ];
        assert_eq!(unusable_text(3, &segments), "1 index and 2 partitions");
        assert_eq!(unusable_text(1, &segments[..1]), "1 index");
        assert_eq!(unusable_text(2, &segments[1..3]), "2 partitions");
    }
}
