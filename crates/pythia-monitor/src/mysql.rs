//! The MySQL dashboard's data: server counters, connections, InnoDB,
//! transactions and lock waits, storage and top statements, read through
//! [`pythia_db::Client`] (the session's `DbRequest::Run`).
//!
//! It comes from `SHOW GLOBAL STATUS` and `VARIABLES`, `information_schema`
//! and `performance_schema`. Sections the user can't read are left out with
//! the privilege they need, as on the Oracle dashboard. Without `PROCESS`,
//! MySQL only lists the user's own threads.

use std::collections::HashMap;

use pythia_db::{Cell, Client, DbError};

use crate::cells::{all_counts, int, num, opt, plural, text, top_counts};
use crate::connections::{Connection, ConnectionList};
use crate::insights::{Fix, FixTask, Insight, Severity};
use crate::queries::SectionError;
use crate::ui::style::{bytes, duration};

macro_rules! counters {
    ($($(#[$doc:meta])* $field:ident: $($var:literal)+;)*) => {
        /// Cumulative counters from `SHOW GLOBAL STATUS`. Between two
        /// snapshots they become rates ([`Counters::per_second`]).
        #[derive(Debug, Clone, Copy, Default, PartialEq)]
        pub struct Counters {
            $($(#[$doc])* pub $field: f64,)*
        }

        impl Counters {
            fn from_status(status: &Status) -> Self {
                Self {
                    $($field: 0.0 $(+ status.num($var))+,)*
                }
            }

            /// Each counter's change per second from `prev` to `cur`.
            pub fn per_second(prev: &Self, cur: &Self, seconds: f64) -> Self {
                if seconds <= 0.0 {
                    return Self::default();
                }
                // Counters start over when the server restarts.
                Self {
                    $($field: (cur.$field - prev.$field).max(0.0) / seconds,)*
                }
            }
        }
    };
}

counters! {
    /// Statements sent by clients.
    queries: "questions";
    selects: "com_select";
    /// INSERT, UPDATE, DELETE and REPLACE statements.
    changes: "com_insert" "com_insert_select" "com_update" "com_update_multi"
        "com_delete" "com_delete_multi" "com_replace" "com_replace_select";
    /// Explicit COMMITs (autocommitted statements aren't counted).
    commits: "com_commit";
    rollbacks: "com_rollback";
    bytes_in: "bytes_received";
    bytes_out: "bytes_sent";
    /// InnoDB page reads from the buffer pool.
    logical_reads: "innodb_buffer_pool_read_requests";
    /// Of those, reads that had to go to disk.
    physical_reads: "innodb_buffer_pool_reads";
    rows_read: "innodb_rows_read";
    rows_changed: "innodb_rows_inserted" "innodb_rows_updated" "innodb_rows_deleted";
    slow_queries: "slow_queries";
    tmp_tables: "created_tmp_tables";
    tmp_disk_tables: "created_tmp_disk_tables";
    row_lock_waits: "innodb_row_lock_waits";
    /// Failed connection attempts (wrong password, no privilege, timeouts).
    aborted_connects: "aborted_connects";
    /// Connections refused because `max_connections` was reached.
    refused_connections: "connection_errors_max_connections";
    /// Queries that scanned a whole table or joined without an index.
    full_scans: "select_scan" "select_full_join";
    /// Bytes written to InnoDB's redo log.
    log_bytes: "innodb_os_log_written";
}

impl Counters {
    /// Share of InnoDB page reads served from memory, in percent.
    pub fn hit_ratio(&self) -> Option<f64> {
        (self.logical_reads > 0.0)
            .then(|| 100.0 * (1.0 - self.physical_reads / self.logical_reads).max(0.0))
    }

    /// Share of internal temporary tables that went to disk, in percent.
    pub fn tmp_disk_pct(&self) -> Option<f64> {
        (self.tmp_tables > 0.0).then(|| 100.0 * self.tmp_disk_tables / self.tmp_tables)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerInfo {
    pub hostname: String,
    pub port: String,
    pub version: String,
    /// E.g. "MySQL Community Server - GPL".
    pub comment: String,
    pub uptime_secs: f64,
    pub read_only: bool,
    pub slow_query_log: bool,
    pub long_query_time: f64,
    pub performance_schema: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Connections {
    pub connected: i64,
    /// Threads executing a statement (not sleeping).
    pub running: i64,
    pub max_used: i64,
    pub max_connections: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BufferPool {
    pub size_bytes: f64,
    pub pages_total: f64,
    pub pages_free: f64,
    pub pages_dirty: f64,
}

impl BufferPool {
    /// Pages in use, in percent.
    pub fn used_pct(&self) -> f64 {
        100.0 * (1.0 - self.pages_free / self.pages_total.max(1.0))
    }

    pub fn dirty_pct(&self) -> f64 {
        100.0 * self.pages_dirty / self.pages_total.max(1.0)
    }
}

/// A client thread running a statement.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Thread {
    pub id: i64,
    pub user: String,
    pub host: String,
    pub db: Option<String>,
    pub command: String,
    pub time_secs: i64,
    pub state: String,
    pub query: Option<String>,
}

/// An open InnoDB transaction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transaction {
    pub thread: i64,
    pub user: Option<String>,
    /// `RUNNING`, `LOCK WAIT`, …
    pub state: String,
    pub age_secs: i64,
    pub rows_locked: i64,
    pub rows_modified: i64,
    /// The statement it runs; none while the client is idle.
    pub query: Option<String>,
    /// The thread is idle (`Sleep`) with the transaction open.
    pub idle: bool,
}

/// A transaction waiting for a row lock another one holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LockWait {
    pub waiting_thread: i64,
    pub wait_secs: i64,
    pub waiting_query: Option<String>,
    pub blocking_thread: i64,
    /// None while the blocking client is idle: likely an uncommitted
    /// transaction.
    pub blocking_query: Option<String>,
    pub table: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Replica {
    pub channel: String,
    pub source: String,
    /// Receiver and applier threads.
    pub io_running: bool,
    pub sql_running: bool,
    /// How long ago the source committed what is being applied.
    pub lag_secs: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaSize {
    pub schema: String,
    pub tables: i64,
    pub data_bytes: f64,
    pub index_bytes: f64,
    /// Allocated but unused (reclaimable with OPTIMIZE TABLE).
    pub free_bytes: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableSize {
    pub schema: String,
    pub name: String,
    pub engine: Option<String>,
    /// InnoDB's estimate.
    pub rows: i64,
    pub data_bytes: f64,
    pub index_bytes: f64,
}

/// Table statistics, refreshed less often than the rest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Storage {
    pub schemas: Vec<SchemaSize>,
    pub tables: Vec<TableSize>,
    /// The table with the most space OPTIMIZE TABLE would give back, when
    /// that's a fair share of it (#145).
    pub reclaimable: Option<Reclaimable>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reclaimable {
    pub schema: String,
    pub name: String,
    pub free_bytes: f64,
    /// Data and indexes.
    pub used_bytes: f64,
}

/// A normalized statement and what its executions cost since the server
/// started (or the statistics were reset).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TopStatement {
    pub schema: Option<String>,
    pub digest: String,
    /// Literals replaced by `?`.
    pub text: String,
    pub executions: i64,
    pub total_secs: f64,
    pub lock_secs: f64,
    pub rows_examined: i64,
    pub rows_sent: i64,
    pub rows_affected: i64,
    /// Executions that used no (good) index.
    pub no_index: i64,
    pub tmp_disk_tables: i64,
}

#[derive(Debug, Clone, Default)]
pub struct MySqlSnapshot {
    pub server: Option<ServerInfo>,
    pub counters: Option<Counters>,
    pub connections: Option<Connections>,
    pub buffer_pool: Option<BufferPool>,
    pub by_user: Vec<(String, i64)>,
    pub by_host: Vec<(String, i64)>,
    pub by_db: Vec<(String, i64)>,
    /// Running threads by state (`executing`, `Waiting for table metadata
    /// lock`, …).
    pub activity: Vec<(String, i64)>,
    /// Running statements, longest first.
    pub running: Vec<Thread>,
    /// Open InnoDB transactions (all of them, not just those listed).
    pub open_transactions: Option<i64>,
    /// The oldest open transactions.
    pub transactions: Vec<Transaction>,
    pub lock_waits: Vec<LockWait>,
    pub replicas: Vec<Replica>,
    /// Only on the snapshots that read table statistics.
    pub storage: Option<Storage>,
    pub top_statements: Vec<TopStatement>,
    /// Without `PROCESS`, only the user's own threads are listed.
    pub own_threads_only: bool,
    pub errors: Vec<SectionError>,
}

/// Collects everything the dashboard shows; never fails as a whole. Table
/// statistics only with `storage`: they cost more on servers with many
/// tables.
pub fn snapshot(c: &mut dyn Client, storage: bool) -> MySqlSnapshot {
    let mut s = MySqlSnapshot::default();
    let mut errors = Vec::new();
    let mut section = |name: &'static str, needs: &str, r: Result<(), DbError>| {
        if let Err(e) = r {
            errors.push((name, explain(&e, needs)));
        }
    };
    section("Server status", "", status(c, &mut s));
    let threads = threads(c, &mut s);
    let threads_listed = threads.is_ok();
    section("Threads", "", threads);
    let transactions = transactions(c, &mut s);
    if transactions.as_ref().is_err_and(is_privilege_error) {
        s.own_threads_only = true;
    }
    section("Transactions", "the PROCESS privilege", transactions);
    if threads_listed
        && !s.own_threads_only
        && let Some(connections) = &mut s.connections
    {
        // Client threads only: no replication or scheduler threads.
        connections.running = s.activity.iter().map(|(_, n)| n).sum();
    }
    if s.transactions.iter().any(|t| t.state == "LOCK WAIT") {
        section(
            "Lock waits",
            "SELECT on performance_schema",
            lock_waits(c, &mut s),
        );
    }
    // Most servers aren't replicas: no need to ask for the privilege.
    if let Err(e) = replication(c, &mut s)
        && !is_privilege_error(&e)
    {
        section("Replication", "", Err(e));
    }
    if storage {
        section("Storage", "", storage_stats(c, &mut s));
    }
    if s.server.as_ref().is_none_or(|i| i.performance_schema) {
        section(
            "Top statements",
            "SELECT on performance_schema",
            top_statements(c, &mut s),
        );
    }
    s.errors = errors;
    s
}

fn is_privilege_error(e: &DbError) -> bool {
    // Access denied to a database, a table, a column; a missing privilege.
    matches!(e.code, Some(1044 | 1142 | 1143 | 1227))
}

/// The error, and for a privilege error what the user would need.
pub fn explain(e: &DbError, needs: &str) -> String {
    if is_privilege_error(e) && !needs.is_empty() {
        format!(
            "Not accessible with the current privileges (needs {needs}). {}",
            e.message
        )
    } else {
        e.message.clone()
    }
}

/// `SHOW` output as name → value, names in lowercase.
struct Status(HashMap<String, String>);

impl Status {
    fn read(c: &mut dyn Client, sql: &str) -> Result<Self, DbError> {
        Ok(Self(
            c.query(sql, &[])?
                .into_iter()
                .filter_map(|row| Some((text(&row, 0).to_lowercase(), row.get(1)?.clone()?)))
                .collect(),
        ))
    }

    fn get(&self, name: &str) -> &str {
        self.0.get(name).map_or("", String::as_str)
    }

    fn num(&self, name: &str) -> f64 {
        self.get(name).parse().unwrap_or(0.0)
    }

    fn on(&self, name: &str) -> bool {
        matches!(self.get(name), "ON" | "1")
    }
}

fn status(c: &mut dyn Client, s: &mut MySqlSnapshot) -> Result<(), DbError> {
    let status = Status::read(c, "show global status")?;
    let vars = Status::read(
        c,
        "show global variables where variable_name in ('hostname', 'port', 'version', \
         'version_comment', 'read_only', 'super_read_only', 'max_connections', \
         'innodb_buffer_pool_size', 'slow_query_log', 'long_query_time', 'performance_schema', \
         'event_scheduler')",
    )?;
    s.server = Some(ServerInfo {
        hostname: vars.get("hostname").to_owned(),
        port: vars.get("port").to_owned(),
        version: vars.get("version").to_owned(),
        comment: vars.get("version_comment").to_owned(),
        uptime_secs: status.num("uptime"),
        read_only: vars.on("read_only") || vars.on("super_read_only"),
        slow_query_log: vars.on("slow_query_log"),
        long_query_time: vars.num("long_query_time"),
        performance_schema: vars.on("performance_schema"),
    });
    s.counters = Some(Counters::from_status(&status));
    s.connections = Some(Connections {
        connected: status.num("threads_connected") as i64,
        // Without the dashboard's own SHOW and the event scheduler's
        // thread; with the whole process list, `snapshot` counts there.
        running: (status.num("threads_running") as i64 - 1 - i64::from(vars.on("event_scheduler")))
            .max(0),
        max_used: status.num("max_used_connections") as i64,
        max_connections: vars.num("max_connections") as i64,
    });
    if status.0.contains_key("innodb_buffer_pool_pages_total") {
        s.buffer_pool = Some(BufferPool {
            size_bytes: vars.num("innodb_buffer_pool_size"),
            pages_total: status.num("innodb_buffer_pool_pages_total"),
            pages_free: status.num("innodb_buffer_pool_pages_free"),
            pages_dirty: status.num("innodb_buffer_pool_pages_dirty"),
        });
    }
    Ok(())
}

/// The replication, scheduler and other threads of the server itself.
fn is_system_thread(user: &str, command: &str) -> bool {
    matches!(user, "system user" | "event_scheduler")
        || matches!(command, "Daemon" | "Binlog Dump" | "Binlog Dump GTID")
}

/// A thread's host without the client's port.
fn client_host(host: &str) -> String {
    match host.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host.to_owned(),
        _ => host.to_owned(),
    }
}

fn threads(c: &mut dyn Client, s: &mut MySqlSnapshot) -> Result<(), DbError> {
    let rows = c.query(
        "select id, user, host, db, command, time, state, info \
           from information_schema.processlist \
          where id <> connection_id()",
        &[],
    )?;
    let threads: Vec<Thread> = rows
        .iter()
        .map(|r| Thread {
            id: int(r, 0),
            user: text(r, 1),
            host: text(r, 2),
            db: opt(r, 3),
            command: text(r, 4),
            time_secs: int(r, 5),
            state: text(r, 6),
            query: opt(r, 7),
        })
        .filter(|t| !is_system_thread(&t.user, &t.command))
        .collect();
    s.by_user = top_counts(threads.iter().map(|t| t.user.clone()));
    s.by_host = top_counts(threads.iter().map(|t| client_host(&t.host)));
    s.by_db = top_counts(
        threads
            .iter()
            .map(|t| t.db.clone().unwrap_or_else(|| "(none)".into())),
    );
    let mut running: Vec<Thread> = threads
        .into_iter()
        .filter(|t| t.command != "Sleep")
        .collect();
    s.activity = all_counts(running.iter().map(|t| {
        if t.state.is_empty() {
            t.command.clone()
        } else {
            t.state.clone()
        }
    }));
    running.sort_by_key(|t| std::cmp::Reverse(t.time_secs));
    running.truncate(10);
    s.running = running;
    Ok(())
}

fn transactions(c: &mut dyn Client, s: &mut MySqlSnapshot) -> Result<(), DbError> {
    let count = c.query("select count(*) from information_schema.innodb_trx", &[])?;
    s.open_transactions = count.first().map(|r| int(r, 0));
    let rows = c.query(
        "select t.trx_mysql_thread_id, p.user, t.trx_state, \
                timestampdiff(second, t.trx_started, now()), t.trx_rows_locked, \
                t.trx_rows_modified, t.trx_query, p.command \
           from information_schema.innodb_trx t \
           left join information_schema.processlist p on p.id = t.trx_mysql_thread_id \
          order by t.trx_started \
          limit 10",
        &[],
    )?;
    s.transactions = rows
        .iter()
        .map(|r| Transaction {
            thread: int(r, 0),
            user: opt(r, 1),
            state: text(r, 2),
            age_secs: int(r, 3),
            rows_locked: int(r, 4),
            rows_modified: int(r, 5),
            query: opt(r, 6),
            idle: opt(r, 7).as_deref() == Some("Sleep"),
        })
        .collect();
    Ok(())
}

fn lock_waits(c: &mut dyn Client, s: &mut MySqlSnapshot) -> Result<(), DbError> {
    const COLUMNS: &str = "r.trx_mysql_thread_id, \
         timestampdiff(second, r.trx_wait_started, now()), r.trx_query, \
         b.trx_mysql_thread_id, b.trx_query";
    let rows = c.query(
        &format!(
            "select {COLUMNS}, concat(l.object_schema, '.', l.object_name) \
               from performance_schema.data_lock_waits w \
               join information_schema.innodb_trx r \
                 on r.trx_id = w.requesting_engine_transaction_id \
               join information_schema.innodb_trx b \
                 on b.trx_id = w.blocking_engine_transaction_id \
               left join performance_schema.data_locks l \
                 on l.engine_lock_id = w.requesting_engine_lock_id \
              order by 2 desc \
              limit 20"
        ),
        &[],
    );
    let rows = match rows {
        // MySQL 5.7 and MariaDB have them in information_schema.
        Err(e) if e.code == Some(1146) => c.query(
            &format!(
                "select {COLUMNS}, replace(l.lock_table, '`', '') \
                   from information_schema.innodb_lock_waits w \
                   join information_schema.innodb_trx r on r.trx_id = w.requesting_trx_id \
                   join information_schema.innodb_trx b on b.trx_id = w.blocking_trx_id \
                   left join information_schema.innodb_locks l \
                     on l.lock_id = w.requested_lock_id \
                  order by 2 desc \
                  limit 20"
            ),
            &[],
        )?,
        rows => rows?,
    };
    s.lock_waits = rows
        .iter()
        .map(|r| LockWait {
            waiting_thread: int(r, 0),
            wait_secs: int(r, 1),
            waiting_query: opt(r, 2),
            blocking_thread: int(r, 3),
            blocking_query: opt(r, 4),
            table: opt(r, 5),
        })
        .collect();
    Ok(())
}

fn replication(c: &mut dyn Client, s: &mut MySqlSnapshot) -> Result<(), DbError> {
    let rows = c.query(
        "select c.channel_name, concat(f.host, ':', f.port), c.service_state, \
                a.service_state, \
                (select timestampdiff(second, \
                            min(nullif(w.applying_transaction_original_commit_timestamp, 0)), \
                            now(6)) \
                   from performance_schema.replication_applier_status_by_worker w \
                  where w.channel_name = c.channel_name and w.applying_transaction <> ''), \
                nullif(c.last_error_message, ''), \
                (select max(nullif(w.last_error_message, '')) \
                   from performance_schema.replication_applier_status_by_worker w \
                  where w.channel_name = c.channel_name) \
           from performance_schema.replication_connection_status c \
           join performance_schema.replication_applier_status a \
             on a.channel_name = c.channel_name \
           left join performance_schema.replication_connection_configuration f \
             on f.channel_name = c.channel_name",
        &[],
    );
    let rows = match rows {
        // MariaDB has only some of these tables.
        Err(e) if e.code == Some(1146) => {
            let rows = c.query("show all slaves status", &[])?;
            s.replicas = rows.iter().map(|r| mariadb_replica(r)).collect();
            return Ok(());
        }
        rows => rows?,
    };
    s.replicas = rows
        .iter()
        .map(|r| Replica {
            channel: text(r, 0),
            source: text(r, 1),
            io_running: text(r, 2) == "ON",
            sql_running: text(r, 3) == "ON",
            // Nothing being applied: caught up.
            lag_secs: Some(opt(r, 4).map_or(0, |_| int(r, 4).max(0))),
            last_error: opt(r, 5).or_else(|| opt(r, 6)),
        })
        .collect();
    Ok(())
}

/// A connection of a MariaDB replica: a row of `SHOW ALL SLAVES STATUS`,
/// by position (the columns up to `Last_SQL_Error` are the same in every
/// version; later ones differ).
fn mariadb_replica(r: &[Cell]) -> Replica {
    let error = |i| opt(r, i).filter(|e| !e.is_empty());
    Replica {
        channel: text(r, 0),
        source: format!("{}:{}", text(r, 3), text(r, 5)),
        io_running: text(r, 12) == "Yes",
        sql_running: text(r, 13) == "Yes",
        // Seconds_Behind_Master: NULL while it doesn't apply.
        lag_secs: opt(r, 34).map(|_| int(r, 34)),
        last_error: error(37).or_else(|| error(39)),
    }
}

/// The schemas of the server itself, left out of the storage lists.
const SYSTEM_SCHEMAS: &str = "('mysql', 'information_schema', 'performance_schema', 'sys')";

fn storage_stats(c: &mut dyn Client, s: &mut MySqlSnapshot) -> Result<(), DbError> {
    // MySQL 8 caches the tables' sizes for a day, read again here every
    // few minutes (#441); MariaDB has no such cache (and no variable).
    let _ = c.execute("set session information_schema_stats_expiry = 0");
    let schemas = c.query(
        &format!(
            "select table_schema, count(*), sum(data_length), sum(index_length), \
                    sum(data_free) \
               from information_schema.tables \
              where table_type = 'BASE TABLE' and table_schema not in {SYSTEM_SCHEMAS} \
              group by table_schema \
              order by sum(data_length) + sum(index_length) desc \
              limit 10"
        ),
        &[],
    )?;
    let tables = c.query(
        &format!(
            "select table_schema, table_name, engine, table_rows, data_length, index_length \
               from information_schema.tables \
              where table_type = 'BASE TABLE' and table_schema not in {SYSTEM_SCHEMAS} \
              order by data_length + index_length desc \
              limit 10"
        ),
        &[],
    )?;
    // InnoDB keeps a few free extents anyway: 32 MB and a fifth of the
    // table, as the maintenance window's line.
    let reclaimable = c.query(
        &format!(
            "select table_schema, table_name, data_free, data_length + index_length \
               from information_schema.tables \
              where table_type = 'BASE TABLE' and table_schema not in {SYSTEM_SCHEMAS} \
                and data_free >= 33554432 and data_free > 0.2 * (data_length + index_length) \
              order by data_free desc \
              limit 1"
        ),
        &[],
    )?;
    s.storage = Some(Storage {
        schemas: schemas
            .iter()
            .map(|r| SchemaSize {
                schema: text(r, 0),
                tables: int(r, 1),
                data_bytes: num(r, 2),
                index_bytes: num(r, 3),
                free_bytes: num(r, 4),
            })
            .collect(),
        tables: tables
            .iter()
            .map(|r| TableSize {
                schema: text(r, 0),
                name: text(r, 1),
                engine: opt(r, 2),
                rows: int(r, 3),
                data_bytes: num(r, 4),
                index_bytes: num(r, 5),
            })
            .collect(),
        reclaimable: reclaimable.first().map(|r| Reclaimable {
            schema: text(r, 0),
            name: text(r, 1),
            free_bytes: num(r, 2),
            used_bytes: num(r, 3),
        }),
    });
    Ok(())
}

fn top_statements(c: &mut dyn Client, s: &mut MySqlSnapshot) -> Result<(), DbError> {
    // Leaves out the dashboard's own lookups (and other dictionary
    // queries): it is about the application's load.
    let rows = c.query(
        "select schema_name, digest, digest_text, count_star, sum_timer_wait / 1e12, \
                sum_lock_time / 1e12, sum_rows_examined, sum_rows_sent, sum_rows_affected, \
                sum_no_index_used + sum_no_good_index_used, sum_created_tmp_disk_tables \
           from performance_schema.events_statements_summary_by_digest \
          where digest_text is not null \
            and digest_text not like 'SHOW %' \
            and digest_text not like '%information_schema%' \
            and digest_text not like '%performance_schema%' \
          order by sum_timer_wait desc \
          limit 10",
        &[],
    )?;
    s.top_statements = rows
        .iter()
        .map(|r| TopStatement {
            schema: opt(r, 0),
            digest: text(r, 1),
            text: text(r, 2),
            executions: int(r, 3),
            total_secs: num(r, 4),
            lock_secs: num(r, 5),
            rows_examined: int(r, 6),
            rows_sent: int(r, 7),
            rows_affected: int(r, 8),
            no_index: int(r, 9),
            tmp_disk_tables: int(r, 10),
        })
        .collect();
    Ok(())
}

/// One thread of the server and what it holds (thread details).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadDetail {
    pub id: i64,
    /// Its processlist row; none when it is gone (or not visible without
    /// `PROCESS`).
    pub thread: Option<Thread>,
    /// How it connected: name, value (`performance_schema.threads`,
    /// `session_connect_attrs`).
    pub connection: Vec<(String, String)>,
    /// The statement it runs, or ran last.
    pub statement: Option<ThreadStatement>,
    /// The statements before, newest first.
    pub history: Vec<ThreadStatement>,
    /// `EXPLAIN FOR CONNECTION` (MariaDB: `SHOW EXPLAIN`) of the running
    /// statement.
    pub plan: Option<String>,
    pub transaction: Option<ThreadTransaction>,
    pub locks: Vec<DataLock>,
    /// The thread whose lock it waits for.
    pub waits_for: Option<i64>,
    /// Metadata locks: object, type, lock type, status.
    pub metadata_locks: Vec<(String, String, String, String)>,
    pub errors: Vec<SectionError>,
}

/// A statement of a thread (`events_statements_current` / `_history`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadStatement {
    pub text: String,
    pub digest: Option<String>,
    /// Seconds; none while it runs.
    pub seconds: Option<f64>,
    pub rows_examined: i64,
    pub rows_sent: i64,
    pub tmp_disk_tables: i64,
    pub sort_rows: i64,
    pub no_index_used: bool,
    pub errors: i64,
    /// Still running.
    pub running: bool,
}

/// A thread's open InnoDB transaction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadTransaction {
    pub state: String,
    pub started: String,
    pub age_secs: i64,
    pub rows_locked: i64,
    pub rows_modified: i64,
    pub isolation: String,
    pub tables_locked: i64,
}

/// A data lock a thread holds or waits for (`performance_schema.data_locks`;
/// MySQL 5.7 and MariaDB: `information_schema.innodb_locks`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataLock {
    pub object: String,
    pub index: Option<String>,
    pub lock_type: String,
    pub mode: String,
    /// `GRANTED` or `WAITING`.
    pub status: String,
    pub data: Option<String>,
}

/// Everything about one thread; each part may fail alone.
pub fn thread_detail(c: &mut dyn Client, id: i64) -> ThreadDetail {
    let mut d = ThreadDetail {
        id,
        ..ThreadDetail::default()
    };
    let mut errors = Vec::new();
    let mut section = |name: &'static str, needs: &str, r: Result<(), DbError>| {
        if let Err(e) = r {
            errors.push((name, explain(&e, needs)));
        }
    };
    let id_text = id.to_string();
    section(
        "Thread",
        "",
        (|| {
            let rows = c.query(
                "select id, user, host, db, command, time, state, info \
                   from information_schema.processlist where id = ?",
                &[&id_text],
            )?;
            d.thread = rows.first().map(|r| Thread {
                id: int(r, 0),
                user: text(r, 1),
                host: text(r, 2),
                db: opt(r, 3),
                command: text(r, 4),
                time_secs: int(r, 5),
                state: text(r, 6),
                query: opt(r, 7),
            });
            Ok(())
        })(),
    );
    let Some(thread) = d.thread.clone() else {
        d.errors = errors;
        return d;
    };
    // performance_schema knows threads by a THREAD_ID of its own.
    let mut ps_thread: Option<String> = None;
    section(
        "Connection",
        "SELECT on performance_schema",
        (|| {
            let rows = c.query(
                "select thread_id, connection_type, thread_os_id \
                   from performance_schema.threads where processlist_id = ?",
                &[&id_text],
            )?;
            if let Some(r) = rows.first() {
                ps_thread = opt(r, 0);
                for (name, i) in [("Connection type", 1), ("OS thread", 2)] {
                    if let Some(v) = opt(r, i) {
                        d.connection.push((name.to_owned(), v));
                    }
                }
            }
            let attrs = c.query(
                "select attr_name, attr_value from performance_schema.session_connect_attrs \
                  where processlist_id = ? order by ordinal_position",
                &[&id_text],
            )?;
            for r in &attrs {
                let name = match text(r, 0).as_str() {
                    "program_name" => "Program",
                    "_client_name" => "Client library",
                    "_client_version" => "Client version",
                    "_os" => "Client OS",
                    "_platform" => "Client platform",
                    "_pid" => "Client process",
                    "os_user" => "OS user",
                    _ => continue,
                };
                d.connection.push((name.to_owned(), text(r, 1)));
            }
            Ok(())
        })(),
    );
    if let Some(ps) = &ps_thread {
        let statement = |r: &[Cell], running: bool| ThreadStatement {
            text: text(r, 0),
            digest: opt(r, 1),
            seconds: opt(r, 2).and_then(|v| v.parse().ok()),
            rows_examined: int(r, 3),
            rows_sent: int(r, 4),
            tmp_disk_tables: int(r, 5),
            sort_rows: int(r, 6),
            no_index_used: int(r, 7) > 0,
            errors: int(r, 8),
            running,
        };
        const COLUMNS: &str = "sql_text, digest_text, timer_wait / 1e12, rows_examined, rows_sent, \
             created_tmp_disk_tables, sort_rows, no_index_used, errors";
        section(
            "Statement",
            "SELECT on performance_schema",
            (|| {
                let rows = c.query(
                    &format!(
                        "select {COLUMNS}, end_event_id is null \
                           from performance_schema.events_statements_current where thread_id = ?"
                    ),
                    &[ps],
                )?;
                d.statement = rows
                    .first()
                    .map(|r| statement(r, int(r, 9) > 0))
                    .filter(|s| !s.text.is_empty());
                let rows = c.query(
                    &format!(
                        "select {COLUMNS} from performance_schema.events_statements_history \
                          where thread_id = ? order by event_id desc limit 10"
                    ),
                    &[ps],
                )?;
                d.history = rows.iter().map(|r| statement(r, false)).collect();
                Ok(())
            })(),
        );
        section(
            "Metadata locks",
            "SELECT on performance_schema",
            (|| {
                let rows = c.query(
                    "select concat_ws('.', object_schema, object_name), object_type, lock_type, lock_status \
                       from performance_schema.metadata_locks where owner_thread_id = ?",
                    &[ps],
                )?;
                d.metadata_locks = rows
                    .iter()
                    .map(|r| (text(r, 0), text(r, 1), text(r, 2), text(r, 3)))
                    .collect();
                Ok(())
            })(),
        );
    }
    section(
        "Locks",
        if ps_thread.is_some() {
            "SELECT on performance_schema"
        } else {
            "the PROCESS privilege"
        },
        thread_locks(c, &id_text, ps_thread.as_deref()).map(|(locks, waits_for)| {
            d.locks = locks;
            d.waits_for = waits_for;
        }),
    );
    section(
        "Transaction",
        "the PROCESS privilege",
        (|| {
            let rows = c.query(
                "select trx_state, trx_started, timestampdiff(second, trx_started, now()), \
                        trx_rows_locked, trx_rows_modified, trx_isolation_level, trx_tables_locked \
                   from information_schema.innodb_trx where trx_mysql_thread_id = ?",
                &[&id_text],
            )?;
            d.transaction = rows.first().map(|r| ThreadTransaction {
                state: text(r, 0),
                started: text(r, 1),
                age_secs: int(r, 2),
                rows_locked: int(r, 3),
                rows_modified: int(r, 4),
                isolation: text(r, 5),
                tables_locked: int(r, 6),
            });
            Ok(())
        })(),
    );
    if thread.command == "Query" && thread.query.is_some() {
        let mariadb = c
            .query("select version()", &[])
            .is_ok_and(|rows| rows.first().is_some_and(|r| text(r, 0).contains("MariaDB")));
        let plan = if mariadb {
            let lock_wait = d
                .transaction
                .as_ref()
                .is_some_and(|t| t.state == "LOCK WAIT");
            mariadb_plan(c, id, lock_wait)
        } else {
            mysql_plan(c, id)
        };
        match plan {
            Ok(plan) if !plan.is_empty() => d.plan = Some(plan),
            Ok(_) => {}
            // Not every statement can be explained (e.g. SLEEP()); say why.
            Err(e) => section("Plan", "the PROCESS privilege", Err(e)),
        }
    }
    d.errors = errors;
    d
}

/// A thread's row locks, and the thread whose lock it waits for: from
/// performance_schema on MySQL 8.0+, by its `THREAD_ID` there; from
/// information_schema on MySQL 5.7 and MariaDB, which lists only the locks
/// of lock waits.
fn thread_locks(
    c: &mut dyn Client,
    id: &str,
    ps_thread: Option<&str>,
) -> Result<(Vec<DataLock>, Option<i64>), DbError> {
    let lock = |r: &Vec<Cell>| DataLock {
        object: text(r, 0),
        index: opt(r, 1),
        lock_type: text(r, 2),
        mode: text(r, 3),
        status: text(r, 4),
        data: opt(r, 5),
    };
    if let Some(ps) = ps_thread {
        let rows = c.query(
            "select concat(object_schema, '.', object_name), index_name, lock_type, lock_mode, \
                    lock_status, lock_data \
               from performance_schema.data_locks where thread_id = ? \
              order by lock_status desc, object_name limit 50",
            &[ps],
        );
        match rows {
            Ok(rows) => {
                let blocker = c.query(
                    "select b.trx_mysql_thread_id \
                       from performance_schema.data_lock_waits w \
                       join information_schema.innodb_trx r \
                         on r.trx_id = w.requesting_engine_transaction_id \
                       join information_schema.innodb_trx b \
                         on b.trx_id = w.blocking_engine_transaction_id \
                      where r.trx_mysql_thread_id = ? limit 1",
                    &[id],
                )?;
                return Ok((
                    rows.iter().map(lock).collect(),
                    blocker.first().map(|r| int(r, 0)),
                ));
            }
            Err(e) if e.code == Some(1146) => {}
            Err(e) => return Err(e),
        }
    }
    let rows = c.query(
        "select replace(l.lock_table, '`', ''), l.lock_index, l.lock_type, l.lock_mode, \
                if(l.lock_id = t.trx_requested_lock_id, 'WAITING', 'GRANTED'), l.lock_data \
           from information_schema.innodb_locks l \
           join information_schema.innodb_trx t on t.trx_id = l.lock_trx_id \
          where t.trx_mysql_thread_id = ? \
          order by 5 desc, 1 limit 50",
        &[id],
    );
    let rows = match rows {
        // MySQL 8.0+ without performance_schema: neither.
        Err(e) if e.code == Some(1109) => return Ok((Vec::new(), None)),
        rows => rows?,
    };
    let blocker = c.query(
        "select b.trx_mysql_thread_id \
           from information_schema.innodb_lock_waits w \
           join information_schema.innodb_trx r on r.trx_id = w.requesting_trx_id \
           join information_schema.innodb_trx b on b.trx_id = w.blocking_trx_id \
          where r.trx_mysql_thread_id = ? limit 1",
        &[id],
    )?;
    Ok((
        rows.iter().map(lock).collect(),
        blocker.first().map(|r| int(r, 0)),
    ))
}

/// `EXPLAIN FOR CONNECTION` of a thread's running statement. The id is a
/// number: safe in the statement. TREE (8.0.16+) reads best; older servers
/// have the table.
fn mysql_plan(c: &mut dyn Client, id: i64) -> Result<String, DbError> {
    c.query(&format!("explain format=tree for connection {id}"), &[])
        .map(|rows| {
            rows.iter()
                .map(|r| text(r, 0))
                .collect::<Vec<_>>()
                .join("\n")
        })
        // The tree only covers what the iterator executor runs (not an
        // UPDATE, for one): the table then.
        .and_then(|tree| {
            if tree.contains("not executable by iterator executor") {
                Err(DbError::new(tree))
            } else {
                Ok(tree)
            }
        })
        .or_else(|_| {
            c.query(&format!("explain for connection {id}"), &[])
                .map(|rows| plan_table(&rows))
        })
}

/// MariaDB's `SHOW EXPLAIN` (10.0+; `EXPLAIN FOR CONNECTION` only from
/// 10.9). The thread hands over its plan itself, between rows: not while it
/// waits for a row lock, where the server would give up only after 30 s.
fn mariadb_plan(c: &mut dyn Client, id: i64, lock_wait: bool) -> Result<String, DbError> {
    if lock_wait {
        return Err(DbError::new(
            "Not available while the thread waits for a row lock: on MariaDB, the thread \
             hands over its plan itself, between rows.",
        ));
    }
    c.query(
        &format!("set statement max_statement_time = 2 for show explain for {id}"),
        &[],
    )
    .map(|rows| plan_table(&rows))
    .map_err(|e| {
        if e.code == Some(1969) {
            DbError::new("The thread didn't hand over its plan within 2 seconds.")
        } else {
            e
        }
    })
}

/// The classic `EXPLAIN` table, a line per row.
fn plan_table(rows: &[Vec<Cell>]) -> String {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|c| c.as_deref().unwrap_or("NULL"))
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every thread, for the Sessions tab: idle ones too, the server's own only
/// with `background`, and who waits for whose row locks.
pub fn connections(c: &mut dyn Client, background: bool) -> Result<ConnectionList, DbError> {
    let mut list = ConnectionList::default();
    let with_transactions = c.query(
        "select p.id, p.user, p.host, p.db, p.command, p.time, p.state, p.info, \
                timestampdiff(second, t.trx_started, now()), t.trx_state, \
                timestampdiff(second, t.trx_wait_started, now()) \
           from information_schema.processlist p \
           left join information_schema.innodb_trx t on t.trx_mysql_thread_id = p.id \
          where p.id <> connection_id()",
        &[],
    );
    let rows = match with_transactions {
        // Without PROCESS: no transactions, and only the user's threads.
        Err(e) if is_privilege_error(&e) => {
            list.notes.push(
                "Only this user's sessions are listed: the others, and open transactions, \
                 need the PROCESS privilege."
                    .into(),
            );
            c.query(
                "select id, user, host, db, command, time, state, info \
                   from information_schema.processlist \
                  where id <> connection_id()",
                &[],
            )?
        }
        rows => rows?,
    };
    let mut lock_wait = false;
    for r in &rows {
        let (user, command) = (text(r, 1), text(r, 4));
        let system = is_system_thread(&user, &command);
        if system && !background {
            continue;
        }
        let waiting = opt(r, 9).as_deref() == Some("LOCK WAIT");
        lock_wait |= waiting;
        list.rows.push(Connection {
            id: int(r, 0),
            user: Some(user),
            host: opt(r, 2).map(|h| client_host(&h)),
            database: opt(r, 3),
            program: None,
            active: command != "Sleep" && !system,
            status: command,
            wait: opt(r, 6).filter(|s| !s.is_empty()),
            seconds: num(r, 5),
            transaction_secs: opt(r, 8).map(|_| num(r, 8)),
            blocked_by: None,
            wait_secs: if waiting { num(r, 10) } else { 0.0 },
            query: opt(r, 7),
            background: system,
        });
    }
    if lock_wait {
        match lock_blockers(c) {
            Ok(pairs) => {
                for (waiting, blocker) in pairs {
                    if let Some(t) = list.rows.iter_mut().find(|t| t.id == waiting) {
                        t.blocked_by.get_or_insert(blocker);
                    }
                }
            }
            Err(e) => list.notes.push(format!(
                "Who blocks whom isn't shown: {}",
                explain(&e, "SELECT on performance_schema")
            )),
        }
    }
    Ok(list)
}

/// Waiting thread and the thread holding the row lock it waits for.
fn lock_blockers(c: &mut dyn Client) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = c.query(
        "select r.trx_mysql_thread_id, b.trx_mysql_thread_id \
           from performance_schema.data_lock_waits w \
           join information_schema.innodb_trx r \
             on r.trx_id = w.requesting_engine_transaction_id \
           join information_schema.innodb_trx b \
             on b.trx_id = w.blocking_engine_transaction_id",
        &[],
    );
    let rows = match rows {
        // MySQL 5.7 and MariaDB have them in information_schema.
        Err(e) if e.code == Some(1146) => c.query(
            "select r.trx_mysql_thread_id, b.trx_mysql_thread_id \
               from information_schema.innodb_lock_waits w \
               join information_schema.innodb_trx r on r.trx_id = w.requesting_trx_id \
               join information_schema.innodb_trx b on b.trx_id = w.blocking_trx_id",
            &[],
        )?,
        rows => rows?,
    };
    Ok(rows.iter().map(|r| (int(r, 0), int(r, 1))).collect())
}

/// Ends a thread's running statement (`query_only`) or its connection.
pub fn kill(c: &mut dyn Client, id: i64, query_only: bool) -> Result<(), DbError> {
    // A number: safe in the statement.
    let what = if query_only { "query " } else { "" };
    c.query(&format!("kill {what}{id}"), &[])?;
    Ok(())
}

/// Findings, most severe first; empty means all is well. `rates` are the
/// counters per second since the previous snapshot.
pub fn insights(snap: &MySqlSnapshot, rates: Option<&Counters>) -> Vec<Insight> {
    let mut out = Vec::new();
    let mut add = |severity, text: String| {
        out.push(Insight {
            severity,
            text,
            target: None,
            session: None,
            fix: None,
        })
    };

    // Lock waits by blocking thread.
    let mut blockers: Vec<(i64, i64, i64, bool)> = Vec::new();
    for w in &snap.lock_waits {
        match blockers.iter_mut().find(|b| b.0 == w.blocking_thread) {
            Some(b) => {
                b.1 += 1;
                b.2 = b.2.max(w.wait_secs);
            }
            None => blockers.push((
                w.blocking_thread,
                1,
                w.wait_secs,
                w.blocking_query.is_none(),
            )),
        }
    }
    for (thread, blocked, wait, idle) in blockers {
        add(
            if wait >= 10 {
                Severity::Critical
            } else {
                Severity::Warning
            },
            format!(
                "Thread {thread} blocks {}, longest wait {}{}",
                plural(blocked, "transaction"),
                duration(wait as f64),
                if idle {
                    " — idle, likely an uncommitted transaction"
                } else {
                    ""
                }
            ),
        );
    }
    let waiting = snap.transactions.iter().filter(|t| t.state == "LOCK WAIT");
    if snap.lock_waits.is_empty() && waiting.clone().count() > 0 {
        add(
            Severity::Warning,
            format!(
                "{} waiting for a row lock",
                plural(waiting.count() as i64, "transaction")
            ),
        );
    }
    let metadata: i64 = snap
        .activity
        .iter()
        .filter(|(state, _)| state.contains("metadata lock"))
        .map(|(_, n)| n)
        .sum();
    if metadata > 0 {
        add(
            Severity::Warning,
            format!(
                "{} waiting for a metadata lock: DDL waits for an open transaction, \
                 and everything after it waits for the DDL",
                plural(metadata, "thread")
            ),
        );
    }

    for r in &snap.replicas {
        let name = if r.channel.is_empty() {
            "Replication".to_owned()
        } else {
            format!("Replication channel {}", r.channel)
        };
        if !(r.io_running && r.sql_running) {
            let stopped = match (r.io_running, r.sql_running) {
                (false, false) => "is stopped",
                (false, true) => "doesn't receive from the source",
                _ => "doesn't apply changes",
            };
            let error = r
                .last_error
                .as_ref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
            add(Severity::Critical, format!("{name} {stopped}{error}"));
        } else if let Some(lag) = r.lag_secs.filter(|l| *l >= 30) {
            add(
                if lag >= 300 {
                    Severity::Critical
                } else {
                    Severity::Warning
                },
                format!("{name} is {} behind the source", duration(lag as f64)),
            );
        }
    }

    if let Some(c) = snap.connections.filter(|c| c.max_connections > 0) {
        let used = c.connected as f64 / c.max_connections as f64;
        if used >= 0.75 {
            add(
                if used >= 0.9 {
                    Severity::Critical
                } else {
                    Severity::Warning
                },
                format!(
                    "{} of {} connections in use ({:.0} %)",
                    c.connected,
                    c.max_connections,
                    used * 100.0
                ),
            );
        }
    }
    if let Some(r) = rates {
        if r.refused_connections > 0.0 {
            add(
                Severity::Critical,
                "Connections are refused: max_connections is reached".into(),
            );
        }
        if r.aborted_connects >= 0.2 {
            add(
                Severity::Warning,
                format!(
                    "{:.1} failed connection attempts per second (wrong passwords, \
                     missing privileges or timeouts)",
                    r.aborted_connects
                ),
            );
        }
        if let Some(hit) = r.hit_ratio().filter(|_| r.logical_reads >= 100.0)
            && hit < 95.0
        {
            add(
                Severity::Warning,
                format!(
                    "Buffer pool hit ratio is {hit:.1} %: {:.0} reads per second go to disk",
                    r.physical_reads
                ),
            );
        }
        if let Some(pct) = r.tmp_disk_pct().filter(|_| r.tmp_tables >= 1.0)
            && pct >= 25.0
        {
            add(
                Severity::Info,
                format!("{pct:.0} % of temporary tables go to disk"),
            );
        }
        if r.slow_queries > 0.0 {
            add(
                Severity::Info,
                format!("{:.1} slow queries per second", r.slow_queries),
            );
        }
    }

    if let Some(t) = snap.transactions.first().filter(|t| t.age_secs >= 60) {
        let who = t.user.as_deref().unwrap_or("?");
        add(
            Severity::Warning,
            format!(
                "Thread {} ({who}) has had a transaction open for {}{}, {} locked",
                t.thread,
                duration(t.age_secs as f64),
                if t.idle { " while idle" } else { "" },
                plural(t.rows_locked, "row")
            ),
        );
    }
    if let Some(t) = snap
        .running
        .first()
        .filter(|t| t.time_secs >= 60 && t.query.is_some())
    {
        add(
            Severity::Info,
            format!(
                "Thread {} ({}) has run a statement for {}",
                t.id,
                t.user,
                duration(t.time_secs as f64)
            ),
        );
    }
    if let Some(b) = snap.buffer_pool.filter(|b| b.dirty_pct() >= 75.0) {
        add(
            Severity::Info,
            format!(
                "{:.0} % of the buffer pool ({}) is waiting to be written",
                b.dirty_pct(),
                bytes(b.size_bytes)
            ),
        );
    }
    let read_only = snap.server.as_ref().is_some_and(|s| s.read_only);
    if read_only {
        add(Severity::Info, "The server is read-only".into());
    }
    if let Some(t) = snap.storage.as_ref().and_then(|s| s.reclaimable.as_ref()) {
        out.push(Insight {
            severity: Severity::Info,
            text: format!(
                "Table {}.{} has {} reclaimable beside {} of data and indexes: OPTIMIZE \
                 TABLE gives it back",
                t.schema,
                t.name,
                bytes(t.free_bytes),
                bytes(t.used_bytes)
            ),
            target: None,
            session: None,
            fix: (!read_only).then(|| Fix::Table {
                schema: t.schema.clone(),
                table: t.name.clone(),
                task: FixTask::Optimize,
            }),
        });
    }
    out.sort_by_key(|i| std::cmp::Reverse(i.severity));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_and_ratios() {
        let status = Status(
            [
                ("questions", "1000"),
                ("com_insert", "10"),
                ("com_update", "5"),
                ("com_delete_multi", "1"),
                ("innodb_buffer_pool_read_requests", "10000"),
                ("innodb_buffer_pool_reads", "100"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        );
        let before = Counters::from_status(&status);
        assert_eq!(before.changes, 16.0);
        let after = Counters {
            queries: 1500.0,
            logical_reads: 30_000.0,
            physical_reads: 1100.0,
            ..before
        };
        let r = Counters::per_second(&before, &after, 10.0);
        assert_eq!(r.queries, 50.0);
        assert_eq!(r.hit_ratio(), Some(95.0));
        assert_eq!(r.tmp_disk_pct(), None);
        // A restarted server's counters start over.
        assert_eq!(Counters::per_second(&after, &before, 10.0).queries, 0.0);
    }

    #[test]
    fn findings() {
        let snap = MySqlSnapshot {
            connections: Some(Connections {
                connected: 140,
                running: 3,
                max_used: 150,
                max_connections: 151,
            }),
            lock_waits: vec![
                LockWait {
                    waiting_thread: 12,
                    wait_secs: 15,
                    blocking_thread: 7,
                    ..Default::default()
                },
                LockWait {
                    waiting_thread: 13,
                    wait_secs: 4,
                    blocking_thread: 7,
                    ..Default::default()
                },
            ],
            activity: vec![("Waiting for table metadata lock".into(), 2)],
            transactions: vec![Transaction {
                thread: 7,
                user: Some("app".into()),
                state: "RUNNING".into(),
                age_secs: 125,
                rows_locked: 1,
                idle: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let rates = Counters {
            logical_reads: 1000.0,
            physical_reads: 200.0,
            ..Default::default()
        };
        let texts: Vec<(Severity, String)> = insights(&snap, Some(&rates))
            .into_iter()
            .map(|i| (i.severity, i.text))
            .collect();
        assert_eq!(
            texts,
            [
                (
                    Severity::Critical,
                    "Thread 7 blocks 2 transactions, longest wait 15s — idle, likely an \
                     uncommitted transaction"
                        .into()
                ),
                (
                    Severity::Critical,
                    "140 of 151 connections in use (93 %)".into()
                ),
                (
                    Severity::Warning,
                    "2 threads waiting for a metadata lock: DDL waits for an open \
                     transaction, and everything after it waits for the DDL"
                        .into()
                ),
                (
                    Severity::Warning,
                    "Buffer pool hit ratio is 80.0 %: 200 reads per second go to disk".into()
                ),
                (
                    Severity::Warning,
                    "Thread 7 (app) has had a transaction open for 2m 5s while idle, 1 row \
                     locked"
                        .into()
                ),
            ]
        );
        assert!(insights(&MySqlSnapshot::default(), None).is_empty());
    }

    /// The table with the most reclaimable space offers OPTIMIZE TABLE
    /// (#145), unless the server is read-only.
    #[test]
    fn reclaimable_space_offers_optimize() {
        let snap = |read_only: bool| MySqlSnapshot {
            server: Some(ServerInfo {
                read_only,
                ..Default::default()
            }),
            storage: Some(Storage {
                reclaimable: Some(Reclaimable {
                    schema: "shop".into(),
                    name: "orders".into(),
                    free_bytes: 300.0 * 1024.0 * 1024.0,
                    used_bytes: 500.0 * 1024.0 * 1024.0,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let found = insights(&snap(false), None);
        let i = found
            .iter()
            .find(|i| i.text.contains("reclaimable"))
            .expect("the finding");
        assert_eq!(
            i.text,
            "Table shop.orders has 300.0 MB reclaimable beside 500.0 MB of data and indexes: \
             OPTIMIZE TABLE gives it back"
        );
        assert_eq!(
            i.fix,
            Some(Fix::Table {
                schema: "shop".into(),
                table: "orders".into(),
                task: FixTask::Optimize,
            })
        );
        assert!(insights(&snap(true), None).iter().all(|i| i.fix.is_none()));
    }
}
