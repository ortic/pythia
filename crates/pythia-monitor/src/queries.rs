//! Monitoring queries against the dynamic performance views.
//!
//! Every function takes a plain `&oracle::Connection` and returns plain data,
//! so it can run inside a [`pythia_db::DbRequest::Task`] on a dedicated
//! monitoring session (or directly in tests). Sections that fail because of
//! missing privileges are reported per section instead of failing the whole
//! snapshot, so the UI can degrade gracefully.

use pythia_db::oracle::sql_type::{FromSql, OracleType, ToSql};
use pythia_db::oracle::{self, Connection};
use pythia_db::{DbError, compat};

pub type Result<T> = std::result::Result<T, DbError>;

/// Hint shown when a dictionary view isn't accessible.
pub const DICTIONARY_GRANT: &str = "SELECT_CATALOG_ROLE or SELECT ANY DICTIONARY";

/// True for "table or view does not exist" / "insufficient privileges".
pub fn is_privilege_error(e: &DbError) -> bool {
    matches!(
        e.code,
        Some(942) | Some(1031) | Some(1039) | Some(6550) | Some(1924)
    )
}

/// A user-facing explanation for a failed section.
pub fn explain_error(e: &DbError) -> String {
    if is_privilege_error(e) {
        format!(
            "Not accessible with the current privileges (needs {DICTIONARY_GRANT}). {}",
            e.message
        )
    } else {
        e.message.clone()
    }
}

fn query<T>(
    conn: &Connection,
    sql: &str,
    params: &[&dyn ToSql],
    mut f: impl FnMut(&oracle::Row) -> Result<T>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in conn.query(sql, params)? {
        out.push(f(&row?)?);
    }
    Ok(out)
}

fn get<T: FromSql>(row: &oracle::Row, idx: usize) -> Result<T> {
    Ok(row.get(idx)?)
}

/// Runs a statement that names objects through `{objects}` in `sql`:
/// `dba_objects`, which lists every schema's objects and is readable with
/// [`DICTIONARY_GRANT`], else `all_objects` (only the objects the user has
/// privileges on).
fn with_objects<T>(sql: &str, mut f: impl FnMut(&str) -> Result<T>) -> Result<T> {
    match f(&sql.replace("{objects}", "dba_objects")) {
        Err(e) if is_privilege_error(&e) => f(&sql.replace("{objects}", "all_objects")),
        r => r,
    }
}

/// Runs a statement with `{sql_text}` in `sql`: the statement's text from
/// `V$SQL`, or (when only `V$SESSION` is granted) none, so sessions still
/// show without it (#90).
fn with_sql_text<T>(sql: &str, mut f: impl FnMut(&str) -> Result<T>) -> Result<T> {
    let from_sql = "(select substr(q.sql_text, 1, 300) from v$sql q \
                    where q.sql_id = s.sql_id and rownum = 1)";
    match f(&sql.replace("{sql_text}", from_sql)) {
        Err(e) if is_privilege_error(&e) => {
            f(&sql.replace("{sql_text}", "cast(null as varchar2(300))"))
        }
        r => r,
    }
}

/// As [`with_objects`], for `{procedures}`.
fn with_procedures<T>(sql: &str, mut f: impl FnMut(&str) -> Result<T>) -> Result<T> {
    match f(&sql.replace("{procedures}", "dba_procedures")) {
        Err(e) if is_privilege_error(&e) => f(&sql.replace("{procedures}", "all_procedures")),
        r => r,
    }
}

// ----- instance ----------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstanceInfo {
    pub instance: String,
    pub host: String,
    pub version: String,
    pub startup: String,
    pub uptime_secs: i64,
    pub database: String,
    pub role: String,
    pub log_mode: String,
    pub cdb: bool,
    pub container: String,
    /// `None` without `V$OSSTAT`.
    pub cpus: Option<i64>,
}

pub fn instance_info(conn: &Connection) -> Result<InstanceInfo> {
    // Containers came with 12.1; from 18c VERSION is only the base release.
    let container = if compat::at_least(conn, 12, 1)? {
        "d.cdb, sys_context('USERENV', 'CON_NAME')"
    } else {
        "'NO', d.name"
    };
    let version = if compat::at_least(conn, 18, 0)? {
        "i.version_full"
    } else {
        "i.version"
    };
    let row = conn.query_row(
        &format!(
            "select i.instance_name, i.host_name, {version}, \
                    to_char(i.startup_time, 'YYYY-MM-DD HH24:MI:SS'), \
                    round((sysdate - i.startup_time) * 86400), \
                    d.name, d.database_role, d.log_mode, {container} \
               from v$instance i cross join v$database d"
        ),
        &[],
    )?;
    Ok(InstanceInfo {
        instance: get(&row, 0)?,
        host: get(&row, 1)?,
        version: get(&row, 2)?,
        startup: get(&row, 3)?,
        uptime_secs: get(&row, 4)?,
        database: get(&row, 5)?,
        role: get(&row, 6)?,
        log_mode: get(&row, 7)?,
        cdb: get::<String>(&row, 8)? == "YES",
        container: get(&row, 9)?,
        // Often not granted with the views above (#90).
        cpus: conn
            .query_row_as::<Option<i64>>(
                "select value from v$osstat where stat_name = 'NUM_CPUS'",
                &[],
            )
            .ok()
            .flatten(),
    })
}

// ----- counters ----------------------------------------------------------

/// Cumulative system counters; rates are computed from two snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Counters {
    pub executions: f64,
    pub logical_reads: f64,
    pub physical_reads: f64,
    pub redo_bytes: f64,
    pub user_commits: f64,
    pub user_rollbacks: f64,
    pub user_calls: f64,
    pub hard_parses: f64,
    /// Centiseconds.
    pub db_time: f64,
    /// Centiseconds.
    pub db_cpu: f64,
    /// OS busy/idle time in centiseconds (all CPUs).
    pub os_busy: f64,
    pub os_idle: f64,
}

pub fn counters(conn: &Connection) -> Result<Counters> {
    let mut c = Counters::default();
    let rows = query(
        conn,
        "select name, value from v$sysstat where name in ('execute count', 'session logical reads', \
         'physical reads', 'redo size', 'user commits', 'user rollbacks', 'user calls', \
         'parse count (hard)', 'DB time', 'CPU used by this session')",
        &[],
        |r| Ok((get::<String>(r, 0)?, get::<f64>(r, 1)?)),
    )?;
    for (name, value) in rows {
        let slot = match name.as_str() {
            "execute count" => &mut c.executions,
            "session logical reads" => &mut c.logical_reads,
            "physical reads" => &mut c.physical_reads,
            "redo size" => &mut c.redo_bytes,
            "user commits" => &mut c.user_commits,
            "user rollbacks" => &mut c.user_rollbacks,
            "user calls" => &mut c.user_calls,
            "parse count (hard)" => &mut c.hard_parses,
            "DB time" => &mut c.db_time,
            "CPU used by this session" => &mut c.db_cpu,
            _ => continue,
        };
        *slot = value;
    }
    // The time model is updated continuously and keeps DB CPU <= DB time;
    // the V$SYSSTAT pair is only a fallback.
    if let Ok(rows) = query(
        conn,
        "select stat_name, value from v$sys_time_model where stat_name in ('DB time', 'DB CPU')",
        &[],
        |r| Ok((get::<String>(r, 0)?, get::<f64>(r, 1)?)),
    ) {
        for (name, micros) in rows {
            match name.as_str() {
                "DB time" => c.db_time = micros / 1e4,
                "DB CPU" => c.db_cpu = micros / 1e4,
                _ => {}
            }
        }
    }
    // V$OSSTAT may be restricted; host CPU is then simply unknown.
    if let Ok(rows) = query(
        conn,
        "select stat_name, value from v$osstat where stat_name in ('BUSY_TIME', 'IDLE_TIME')",
        &[],
        |r| Ok((get::<String>(r, 0)?, get::<f64>(r, 1)?)),
    ) {
        for (name, value) in rows {
            match name.as_str() {
                "BUSY_TIME" => c.os_busy = value,
                "IDLE_TIME" => c.os_idle = value,
                _ => {}
            }
        }
    }
    Ok(c)
}

/// Per-second rates between two counter snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rates {
    /// Average active sessions (DB time per second).
    pub aas: f64,
    /// Database CPU in average sessions on CPU.
    pub db_cpu: f64,
    /// Host CPU utilization in percent, if V$OSSTAT is readable.
    pub host_cpu_pct: Option<f64>,
    pub executions: f64,
    pub logical_reads: f64,
    pub physical_reads: f64,
    pub redo_bytes: f64,
    pub commits: f64,
    pub rollbacks: f64,
    pub user_calls: f64,
    pub hard_parses: f64,
}

impl Rates {
    pub fn between(prev: &Counters, cur: &Counters, seconds: f64) -> Self {
        if seconds <= 0.0 {
            return Self::default();
        }
        // Counters can reset (instance restart); clamp negative deltas.
        let d = |a: f64, b: f64| ((b - a).max(0.0)) / seconds;
        let busy = cur.os_busy - prev.os_busy;
        let idle = cur.os_idle - prev.os_idle;
        let host_cpu_pct = (busy + idle > 0.0 && busy >= 0.0).then(|| 100.0 * busy / (busy + idle));
        Self {
            aas: d(prev.db_time, cur.db_time) / 100.0,
            db_cpu: d(prev.db_cpu, cur.db_cpu) / 100.0,
            host_cpu_pct,
            executions: d(prev.executions, cur.executions),
            logical_reads: d(prev.logical_reads, cur.logical_reads),
            physical_reads: d(prev.physical_reads, cur.physical_reads),
            redo_bytes: d(prev.redo_bytes, cur.redo_bytes),
            commits: d(prev.user_commits, cur.user_commits),
            rollbacks: d(prev.user_rollbacks, cur.user_rollbacks),
            user_calls: d(prev.user_calls, cur.user_calls),
            hard_parses: d(prev.hard_parses, cur.hard_parses),
        }
    }
}

// ----- dashboard ---------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCounts {
    pub total: i64,
    pub user: i64,
    pub active: i64,
    pub inactive: i64,
    pub background: i64,
    pub blocked: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorHolder {
    pub sid: i64,
    pub serial: i64,
    pub username: Option<String>,
    pub program: Option<String>,
    pub open: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tablespace {
    pub name: String,
    pub used_pct: f64,
    pub used_bytes: f64,
    pub max_bytes: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TopSql {
    pub sql_id: String,
    pub executions: i64,
    pub elapsed_s: f64,
    pub cpu_s: f64,
    pub buffer_gets: i64,
    pub disk_reads: i64,
    pub rows: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LongOp {
    pub sid: i64,
    pub serial: i64,
    pub opname: String,
    pub target: Option<String>,
    pub sofar: f64,
    pub totalwork: f64,
    pub units: Option<String>,
    pub elapsed_s: i64,
    pub remaining_s: Option<i64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Memory {
    pub sga: Vec<(String, f64)>,
    pub pga_allocated: f64,
    pub pga_inuse: f64,
    pub pga_target: f64,
}

/// A failed dashboard section: (section name, explanation).
pub type SectionError = (&'static str, String);

/// A session at the head of a blocking chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Blocker {
    pub sid: i64,
    pub serial: i64,
    pub username: Option<String>,
    pub status: String,
    /// Sessions waiting on it, directly or indirectly.
    pub blocked: i64,
    /// Longest current wait among them, in seconds.
    pub max_wait: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DashboardSnapshot {
    pub instance: Option<InstanceInfo>,
    pub counters: Option<Counters>,
    pub sessions: Option<SessionCounts>,
    /// Final blockers, most blocking first.
    pub blockers: Vec<Blocker>,
    pub by_user: Vec<(String, i64)>,
    pub by_machine: Vec<(String, i64)>,
    pub by_program: Vec<(String, i64)>,
    /// Active user sessions by wait class (`CPU` for sessions on CPU).
    pub activity: Vec<(String, i64)>,
    pub open_cursors: Option<i64>,
    /// The `open_cursors` parameter (per-session limit).
    pub open_cursors_limit: Option<i64>,
    pub cursor_holders: Vec<CursorHolder>,
    pub memory: Option<Memory>,
    pub tablespaces: Vec<Tablespace>,
    pub top_sql: Vec<TopSql>,
    pub long_ops: Vec<LongOp>,
    /// Non-idle wait events since startup, for rates between snapshots.
    pub events: Vec<SystemEvent>,
    pub errors: Vec<SectionError>,
}

/// Collects everything the dashboard shows. Never fails as a whole.
pub fn dashboard_snapshot(conn: &Connection) -> DashboardSnapshot {
    let mut s = DashboardSnapshot::default();
    let section = |name: &'static str, errors: &mut Vec<SectionError>, r: Result<()>| {
        if let Err(e) = r {
            errors.push((name, explain_error(&e)));
        }
    };
    let mut errors = Vec::new();

    section(
        "Instance",
        &mut errors,
        instance_info(conn).map(|i| s.instance = Some(i)),
    );
    section(
        "Counters",
        &mut errors,
        counters(conn).map(|c| s.counters = Some(c)),
    );
    section(
        "Sessions",
        &mut errors,
        (|| {
            let r = conn.query_row(
                "select count(*), count(case when type = 'USER' then 1 end), \
                        count(case when type = 'USER' and status = 'ACTIVE' then 1 end), \
                        count(case when type = 'USER' and status = 'INACTIVE' then 1 end), \
                        count(case when type = 'BACKGROUND' then 1 end), \
                        count(case when blocking_session is not null then 1 end) \
                   from v$session",
                &[],
            )?;
            s.sessions = Some(SessionCounts {
                total: get(&r, 0)?,
                user: get(&r, 1)?,
                active: get(&r, 2)?,
                inactive: get(&r, 3)?,
                background: get(&r, 4)?,
                blocked: get(&r, 5)?,
            });
            let group = |col: &str| {
                query(
                    conn,
                    &compat::first_rows(
                        &format!(
                            "select nvl({col}, '(unknown)'), count(*) from v$session \
                              where type = 'USER' group by {col} order by 2 desc, 1"
                        ),
                        8,
                    ),
                    &[],
                    |r| Ok((get(r, 0)?, get(r, 1)?)),
                )
            };
            s.blockers = blockers(conn)?;
            s.by_user = group("username")?;
            s.by_machine = group("machine")?;
            s.by_program = group("program")?;
            s.activity = query(
                conn,
                "select case when state = 'WAITING' then wait_class else 'CPU' end, count(*) \
                   from v$session \
                  where type = 'USER' and status = 'ACTIVE' \
                    and (state <> 'WAITING' or wait_class <> 'Idle') \
                    and sid <> sys_context('USERENV', 'SID') \
                  group by case when state = 'WAITING' then wait_class else 'CPU' end \
                  order by 2 desc",
                &[],
                |r| Ok((get(r, 0)?, get(r, 1)?)),
            )?;
            Ok(())
        })(),
    );
    section(
        "Open cursors",
        &mut errors,
        (|| {
            s.open_cursors = Some(conn.query_row_as::<i64>(
                "select value from v$sysstat where name = 'opened cursors current'",
                &[],
            )?);
            s.open_cursors_limit = conn
                .query_row_as::<Option<i64>>(
                    "select to_number(value) from v$parameter where name = 'open_cursors'",
                    &[],
                )
                .ok()
                .flatten();
            s.cursor_holders = query(
                conn,
                &compat::first_rows(
                    "select s.sid, s.serial#, s.username, s.program, st.value \
                       from v$sesstat st \
                       join v$statname n on n.statistic# = st.statistic# \
                       join v$session s on s.sid = st.sid \
                      where n.name = 'opened cursors current' and st.value > 0 \
                      order by st.value desc",
                    10,
                ),
                &[],
                |r| {
                    Ok(CursorHolder {
                        sid: get(r, 0)?,
                        serial: get(r, 1)?,
                        username: get(r, 2)?,
                        program: get(r, 3)?,
                        open: get(r, 4)?,
                    })
                },
            )?;
            Ok(())
        })(),
    );
    section(
        "Memory",
        &mut errors,
        memory(conn).map(|m| s.memory = Some(m)),
    );
    section(
        "Tablespaces",
        &mut errors,
        tablespaces(conn).map(|t| s.tablespaces = t),
    );
    section(
        "Top SQL",
        &mut errors,
        top_sql(conn, 15).map(|t| s.top_sql = t),
    );
    section(
        "Long operations",
        &mut errors,
        long_ops(conn).map(|l| s.long_ops = l),
    );
    section(
        "Wait events",
        &mut errors,
        system_events(conn).map(|e| s.events = e),
    );
    s.errors = errors;
    s
}

/// What a tablespace holds: its files, and its largest segments or, for a
/// temporary one, the sessions using it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TablespaceDetail {
    pub temporary: bool,
    pub files: Vec<DataFile>,
    /// `OWNER.NAME`, segment type, bytes; largest first.
    pub segments: Vec<(String, String, f64)>,
    /// sid, serial#, user, segment type (SORT, HASH, …), bytes.
    pub temp_users: Vec<(i64, i64, Option<String>, String, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataFile {
    pub name: String,
    pub bytes: f64,
    /// What it may grow to (0 when it doesn't).
    pub max_bytes: f64,
    pub autoextensible: bool,
}

pub fn tablespace_detail(conn: &Connection, name: &str) -> Result<TablespaceDetail> {
    let contents: Option<String> = query(
        conn,
        "select contents from dba_tablespaces where tablespace_name = :1",
        &[&name],
        |r| get(r, 0),
    )?
    .into_iter()
    .next();
    let temporary = contents.as_deref() == Some("TEMPORARY");
    let files = if temporary {
        "dba_temp_files"
    } else {
        "dba_data_files"
    };
    let mut d = TablespaceDetail {
        temporary,
        files: query(
            conn,
            &format!(
                "select file_name, bytes, maxbytes, autoextensible from {files} \
                  where tablespace_name = :1 order by file_id"
            ),
            &[&name],
            |r| {
                Ok(DataFile {
                    name: get(r, 0)?,
                    bytes: get::<Option<f64>>(r, 1)?.unwrap_or(0.0),
                    max_bytes: get::<Option<f64>>(r, 2)?.unwrap_or(0.0),
                    autoextensible: get::<Option<String>>(r, 3)?.as_deref() == Some("YES"),
                })
            },
        )?,
        ..TablespaceDetail::default()
    };
    if temporary {
        d.temp_users = query(
            conn,
            "select s.sid, s.serial#, s.username, u.segtype, u.blocks * t.block_size \
               from v$tempseg_usage u \
               join v$session s on s.saddr = u.session_addr \
               join dba_tablespaces t on t.tablespace_name = u.tablespace \
              where u.tablespace = :1 order by 5 desc",
            &[&name],
            |r| {
                Ok((
                    get(r, 0)?,
                    get(r, 1)?,
                    get(r, 2)?,
                    get::<Option<String>>(r, 3)?.unwrap_or_default(),
                    get::<Option<f64>>(r, 4)?.unwrap_or(0.0),
                ))
            },
        )?;
    } else {
        d.segments = query(
            conn,
            &compat::first_rows(
                "select owner || '.' || segment_name, segment_type, bytes from dba_segments \
                  where tablespace_name = :1 order by bytes desc",
                10,
            ),
            &[&name],
            |r| {
                Ok((
                    get(r, 0)?,
                    get::<Option<String>>(r, 1)?.unwrap_or_default(),
                    get::<Option<f64>>(r, 2)?.unwrap_or(0.0),
                ))
            },
        )?;
    }
    Ok(d)
}

/// A session and its PGA, in bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct PgaUser {
    pub sid: i64,
    pub serial: i64,
    pub user: Option<String>,
    pub program: Option<String>,
    pub used: f64,
    pub allocated: f64,
}

/// The sessions using the most PGA.
pub fn pga_users(conn: &Connection) -> Result<Vec<PgaUser>> {
    query(
        conn,
        &compat::first_rows(
            "select s.sid, s.serial#, s.username, s.program, p.pga_used_mem, p.pga_alloc_mem \
               from v$session s join v$process p on p.addr = s.paddr \
              order by p.pga_alloc_mem desc",
            10,
        ),
        &[],
        |r| {
            Ok(PgaUser {
                sid: get(r, 0)?,
                serial: get(r, 1)?,
                user: get(r, 2)?,
                program: get(r, 3)?,
                used: get::<Option<f64>>(r, 4)?.unwrap_or(0.0),
                allocated: get::<Option<f64>>(r, 5)?.unwrap_or(0.0),
            })
        },
    )
}

/// A wait event's totals since startup (`V$SYSTEM_EVENT`).
#[derive(Debug, Clone, PartialEq)]
pub struct SystemEvent {
    pub event: String,
    pub wait_class: String,
    pub waits: f64,
    pub micros: f64,
}

/// The non-idle wait events.
pub fn system_events(conn: &Connection) -> Result<Vec<SystemEvent>> {
    query(
        conn,
        "select event, wait_class, total_waits, time_waited_micro \
           from v$system_event where wait_class <> 'Idle'",
        &[],
        |r| {
            Ok(SystemEvent {
                event: get(r, 0)?,
                wait_class: get::<Option<String>>(r, 1)?.unwrap_or_default(),
                waits: get::<Option<f64>>(r, 2)?.unwrap_or(0.0),
                micros: get::<Option<f64>>(r, 3)?.unwrap_or(0.0),
            })
        },
    )
}

/// A wait event between two snapshots.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRate {
    pub event: String,
    pub wait_class: String,
    pub waits_per_s: f64,
    /// Seconds waited per second: sessions waiting on it, on average.
    pub load: f64,
    pub average_ms: f64,
}

/// The events waited on between two snapshots `secs` apart, most waited
/// first.
pub fn event_rates(prev: &[SystemEvent], cur: &[SystemEvent], secs: f64) -> Vec<EventRate> {
    let mut out: Vec<EventRate> = cur
        .iter()
        .filter_map(|c| {
            let p = prev.iter().find(|p| p.event == c.event);
            let (waits, micros) = match p {
                Some(p) => (c.waits - p.waits, c.micros - p.micros),
                // New since the last snapshot.
                None => (c.waits, c.micros),
            };
            (micros > 0.0 && secs > 0.0).then(|| EventRate {
                event: c.event.clone(),
                wait_class: c.wait_class.clone(),
                waits_per_s: waits / secs,
                load: micros / 1e6 / secs,
                average_ms: if waits > 0.0 {
                    micros / 1000.0 / waits
                } else {
                    0.0
                },
            })
        })
        .collect();
    out.sort_by(|a, b| b.load.total_cmp(&a.load));
    out
}

/// How long the waits on an event took since startup: (up to this many
/// milliseconds, waits).
pub fn event_histogram(conn: &Connection, event: &str) -> Result<Vec<(f64, i64)>> {
    query(
        conn,
        "select wait_time_milli, wait_count from v$event_histogram \
          where event = :1 order by wait_time_milli",
        &[&event],
        |r| Ok((get(r, 0)?, get(r, 1)?)),
    )
}

/// Heads of blocking chains, most blocking first.
///
/// Uses `BLOCKING_SESSION` (populated everywhere) rather than
/// `FINAL_BLOCKING_SESSION`, which some versions leave empty in a PDB.
pub fn blockers(conn: &Connection) -> Result<Vec<Blocker>> {
    let rows = query(
        conn,
        "select sid, serial#, username, status, blocking_session, \
                case when state = 'WAITING' then wait_time_micro / 1e6 else 0 end \
           from v$session \
          where blocking_session is not null \
             or sid in (select blocking_session from v$session where blocking_session is not null)",
        &[],
        |r| {
            Ok(SessionRow {
                sid: get(r, 0)?,
                serial: get(r, 1)?,
                username: get(r, 2)?,
                status: get::<Option<String>>(r, 3)?.unwrap_or_default(),
                blocking_session: get(r, 4)?,
                wait_secs: get::<Option<f64>>(r, 5)?.unwrap_or(0.0),
                ..SessionRow::default()
            })
        },
    )?;
    Ok(build_blocking_tree(&rows)
        .into_iter()
        .map(|root| Blocker {
            blocked: root.blocked_count() as i64,
            max_wait: root.max_wait(),
            sid: root.session.sid,
            serial: root.session.serial,
            username: root.session.username,
            status: root.session.status,
        })
        .collect())
}

pub fn memory(conn: &Connection) -> Result<Memory> {
    let sga = query(
        conn,
        "select name, value from v$sga order by value desc",
        &[],
        |r| Ok((get(r, 0)?, get(r, 1)?)),
    )?;
    let mut m = Memory {
        sga,
        ..Memory::default()
    };
    for (name, value) in query(
        conn,
        "select name, value from v$pgastat where name in \
         ('total PGA allocated', 'total PGA inuse', 'aggregate PGA target parameter')",
        &[],
        |r| Ok((get::<String>(r, 0)?, get::<f64>(r, 1)?)),
    )? {
        match name.as_str() {
            "total PGA allocated" => m.pga_allocated = value,
            "total PGA inuse" => m.pga_inuse = value,
            _ => m.pga_target = value,
        }
    }
    Ok(m)
}

pub fn tablespaces(conn: &Connection) -> Result<Vec<Tablespace>> {
    query(
        conn,
        "select m.tablespace_name, m.used_percent, m.used_space * t.block_size, m.tablespace_size * t.block_size \
           from dba_tablespace_usage_metrics m \
           join dba_tablespaces t on t.tablespace_name = m.tablespace_name \
          order by m.used_percent desc",
        &[],
        |r| {
            Ok(Tablespace {
                name: get(r, 0)?,
                used_pct: get(r, 1)?,
                used_bytes: get(r, 2)?,
                max_bytes: get(r, 3)?,
            })
        },
    )
}

pub fn top_sql(conn: &Connection, limit: usize) -> Result<Vec<TopSql>> {
    query(
        conn,
        &compat::first_rows(
            "select sql_id, executions, elapsed_time / 1e6, cpu_time / 1e6, buffer_gets, disk_reads, \
                    rows_processed, substr(sql_text, 1, 300) \
               from v$sqlstats order by elapsed_time desc",
            limit,
        ),
        &[],
        |r| {
            Ok(TopSql {
                sql_id: get(r, 0)?,
                executions: get(r, 1)?,
                elapsed_s: get(r, 2)?,
                cpu_s: get(r, 3)?,
                buffer_gets: get(r, 4)?,
                disk_reads: get(r, 5)?,
                rows: get(r, 6)?,
                text: get::<Option<String>>(r, 7)?.unwrap_or_default(),
            })
        },
    )
}

pub fn long_ops(conn: &Connection) -> Result<Vec<LongOp>> {
    long_ops_of(conn, None)
}

/// Long operations in progress: of every session, or of one (its finished
/// ones too).
fn long_ops_of(conn: &Connection, session: Option<(i64, i64)>) -> Result<Vec<LongOp>> {
    let (filter, params): (&str, Vec<&dyn ToSql>) = match &session {
        Some((sid, serial)) => ("sid = :1 and serial# = :2", vec![sid, serial]),
        None => ("sofar < totalwork", Vec::new()),
    };
    query(
        conn,
        &compat::first_rows(
            &format!(
                "select sid, serial#, opname, target, sofar, totalwork, units, elapsed_seconds, time_remaining, message \
                   from v$session_longops \
                  where {filter} \
                  order by start_time desc"
            ),
            20,
        ),
        &params,
        |r| {
            Ok(LongOp {
                sid: get(r, 0)?,
                serial: get(r, 1)?,
                opname: get::<Option<String>>(r, 2)?.unwrap_or_default(),
                target: get(r, 3)?,
                sofar: get::<Option<f64>>(r, 4)?.unwrap_or(0.0),
                totalwork: get::<Option<f64>>(r, 5)?.unwrap_or(0.0),
                units: get(r, 6)?,
                elapsed_s: get::<Option<i64>>(r, 7)?.unwrap_or(0),
                remaining_s: get(r, 8)?,
                message: get(r, 9)?,
            })
        },
    )
}

// ----- sessions ----------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionRow {
    pub sid: i64,
    pub serial: i64,
    pub username: Option<String>,
    pub status: String,
    pub osuser: Option<String>,
    pub machine: Option<String>,
    pub program: Option<String>,
    pub module: Option<String>,
    pub action: Option<String>,
    pub sql_id: Option<String>,
    pub prev_sql_id: Option<String>,
    pub event: Option<String>,
    pub wait_class: Option<String>,
    /// `WAITING`, `WAITED KNOWN TIME`, ...
    pub state: Option<String>,
    /// Seconds in the current wait (0 when on CPU).
    pub wait_secs: f64,
    pub blocking_session: Option<i64>,
    pub final_blocking_session: Option<i64>,
    pub logon_time: String,
    /// Seconds since the last call started (active) or ended (inactive).
    pub last_call_et: i64,
    pub background: bool,
    /// Object waited on (for row lock waits), `OWNER.NAME`.
    pub wait_object: Option<String>,
    /// Row waited on, if the session waits for a row lock.
    pub wait_rowid: Option<String>,
    pub sql_text: Option<String>,
    /// The monitoring session itself.
    pub is_self: bool,
    /// SQL trace is on (`V$SESSION.SQL_TRACE`).
    pub sql_trace: bool,
    /// Serial# of the session it waits for, to open its details.
    pub blocking_serial: Option<i64>,
    /// Child cursors of its current and previous SQL (plan, binds).
    pub sql_child: Option<i64>,
    pub prev_child: Option<i64>,
}

impl SessionRow {
    pub fn is_blocked(&self) -> bool {
        self.blocking_session.is_some()
    }

    pub fn is_active(&self) -> bool {
        self.status == "ACTIVE"
    }

    /// A blocker the session list doesn't have, in the blocking tree: only
    /// its SID is known (see [`SessionRow::unknown`]).
    pub fn is_unknown(&self) -> bool {
        self.status == UNKNOWN
    }

    fn unknown(sid: i64) -> Self {
        SessionRow {
            sid,
            status: UNKNOWN.into(),
            ..SessionRow::default()
        }
    }

    /// `sid,serial#`, as used in `ALTER SYSTEM KILL SESSION`.
    pub fn handle(&self) -> String {
        format!("{},{}", self.sid, self.serial)
    }
}

/// Status of [`SessionRow::unknown`] (not one of `V$SESSION.STATUS`).
const UNKNOWN: &str = "UNKNOWN";

const SESSION_SELECT: &str = "\
select s.sid, s.serial#, s.username, s.status, s.osuser, s.machine, s.program, s.module, s.action, \
       s.sql_id, s.prev_sql_id, s.event, s.wait_class, s.state, \
       case when s.state = 'WAITING' then s.wait_time_micro / 1e6 else 0 end, \
       s.blocking_session, s.final_blocking_session, \
       to_char(s.logon_time, 'YYYY-MM-DD HH24:MI:SS'), s.last_call_et, s.type, \
       case when s.row_wait_obj# > 0 then o.owner || '.' || o.object_name end, \
       case when s.row_wait_obj# > 0 and s.event like 'enq: TX%' and o.data_object_id is not null then \
         rowidtochar(dbms_rowid.rowid_create(1, o.data_object_id, s.row_wait_file#, s.row_wait_block#, s.row_wait_row#)) end, \
       {sql_text}, \
       case when s.sid = sys_context('USERENV', 'SID') then 1 else 0 end, \
       case when s.sql_trace = 'ENABLED' then 1 else 0 end, \
       (select b.serial# from v$session b where b.sid = s.blocking_session), \
       s.sql_child_number, s.prev_child_number \
  from v$session s \
  left join {objects} o on o.object_id = s.row_wait_obj#";

fn session_row(r: &oracle::Row) -> Result<SessionRow> {
    Ok(SessionRow {
        sid: get(r, 0)?,
        serial: get(r, 1)?,
        username: get(r, 2)?,
        status: get::<Option<String>>(r, 3)?.unwrap_or_default(),
        osuser: get(r, 4)?,
        machine: get(r, 5)?,
        program: get(r, 6)?,
        module: get(r, 7)?,
        action: get(r, 8)?,
        sql_id: get(r, 9)?,
        prev_sql_id: get(r, 10)?,
        event: get(r, 11)?,
        wait_class: get(r, 12)?,
        state: get(r, 13)?,
        wait_secs: get::<Option<f64>>(r, 14)?.unwrap_or(0.0),
        blocking_session: get(r, 15)?,
        final_blocking_session: get(r, 16)?,
        logon_time: get::<Option<String>>(r, 17)?.unwrap_or_default(),
        last_call_et: get::<Option<i64>>(r, 18)?.unwrap_or(0),
        background: get::<Option<String>>(r, 19)?.as_deref() == Some("BACKGROUND"),
        wait_object: get(r, 20)?,
        wait_rowid: get(r, 21)?,
        sql_text: get(r, 22)?,
        is_self: get::<i64>(r, 23)? == 1,
        sql_trace: get::<i64>(r, 24)? == 1,
        blocking_serial: get(r, 25)?,
        sql_child: get(r, 26)?,
        prev_child: get(r, 27)?,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionFilter {
    pub include_background: bool,
}

pub fn sessions(conn: &Connection, filter: SessionFilter) -> Result<Vec<SessionRow>> {
    let sql = if filter.include_background {
        format!("{SESSION_SELECT} order by s.sid")
    } else {
        format!("{SESSION_SELECT} where s.type = 'USER' order by s.sid")
    };
    with_objects(&sql, |sql| {
        with_sql_text(sql, |sql| query(conn, sql, &[], session_row))
    })
}

/// A session and the sessions it blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockingNode {
    pub session: SessionRow,
    pub children: Vec<BlockingNode>,
}

impl BlockingNode {
    /// Number of sessions waiting on this one, directly or indirectly.
    pub fn blocked_count(&self) -> usize {
        self.children.iter().map(|c| 1 + c.blocked_count()).sum()
    }

    /// Longest wait in this subtree, in seconds.
    pub fn max_wait(&self) -> f64 {
        self.children
            .iter()
            .map(|c| c.session.wait_secs.max(c.max_wait()))
            .fold(0.0, f64::max)
    }
}

/// Builds the blocker → waiter forest from a session list.
///
/// Roots are sessions that block others but aren't blocked themselves (the
/// final blockers). A waiter whose blocker isn't in the list (e.g. another
/// instance, or a background session filtered out) gets a placeholder root.
/// Cycles (deadlocks in progress) are broken at the session with the lowest SID.
pub fn build_blocking_tree(sessions: &[SessionRow]) -> Vec<BlockingNode> {
    use std::collections::{HashMap, HashSet};
    let by_sid: HashMap<i64, &SessionRow> = sessions.iter().map(|s| (s.sid, s)).collect();
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    for s in sessions {
        if let Some(b) = s.blocking_session {
            children.entry(b).or_default().push(s.sid);
        }
    }
    for list in children.values_mut() {
        list.sort_unstable();
    }

    // Blockers that aren't blocked (or whose blocker is unknown) are roots.
    let mut roots: Vec<i64> = children
        .keys()
        .copied()
        .filter(
            |sid| match by_sid.get(sid).and_then(|s| s.blocking_session) {
                None => true,
                Some(b) => !by_sid.contains_key(&b) && !children.contains_key(&b),
            },
        )
        .collect();
    // Blockers referenced but missing from the list.
    roots.sort_unstable();

    fn build(
        sid: i64,
        by_sid: &HashMap<i64, &SessionRow>,
        children: &HashMap<i64, Vec<i64>>,
        seen: &mut HashSet<i64>,
    ) -> BlockingNode {
        seen.insert(sid);
        let session = by_sid
            .get(&sid)
            .map(|s| (*s).clone())
            .unwrap_or_else(|| SessionRow::unknown(sid));
        let kids = children
            .get(&sid)
            .into_iter()
            .flatten()
            .filter(|c| !seen.contains(c))
            .copied()
            .collect::<Vec<_>>();
        let children = kids
            .into_iter()
            .map(|c| build(c, by_sid, children, seen))
            .collect();
        BlockingNode { session, children }
    }

    let mut seen = HashSet::new();
    let mut forest: Vec<BlockingNode> = roots
        .into_iter()
        .map(|sid| build(sid, &by_sid, &children, &mut seen))
        .collect();
    // Anything left is part of a cycle.
    let mut rest: Vec<i64> = children
        .keys()
        .copied()
        .filter(|s| !seen.contains(s))
        .collect();
    rest.sort_unstable();
    for sid in rest {
        if !seen.contains(&sid) {
            forest.push(build(sid, &by_sid, &children, &mut seen));
        }
    }
    forest.sort_by(|a, b| {
        b.blocked_count()
            .cmp(&a.blocked_count())
            .then(a.session.sid.cmp(&b.session.sid))
    });
    forest
}

pub fn blocking_tree(conn: &Connection) -> Result<Vec<BlockingNode>> {
    Ok(build_blocking_tree(&sessions(
        conn,
        SessionFilter {
            include_background: true,
        },
    )?))
}

// ----- session detail ----------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct WaitHistory {
    pub seq: i64,
    pub event: String,
    pub wait_ms: f64,
    pub params: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LockRow {
    pub lock_type: String,
    pub id1: i64,
    pub id2: i64,
    pub mode_held: String,
    pub mode_requested: String,
    pub seconds: i64,
    pub blocking: bool,
    pub object: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenCursor {
    pub sql_id: Option<String>,
    /// The whole statement, or the start the instance still has of it (all
    /// of it while it's in the shared pool).
    pub text: String,
    pub cursor_type: Option<String>,
}

/// The parts of session details, each on a tab of its own and read when
/// it is first shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum DetailTab {
    #[default]
    Overview,
    Sql,
    Plan,
    Waits,
    Stats,
    Cursors,
    Locks,
    Columns,
    /// What the management packs keep (read by the view itself).
    History,
}

impl DetailTab {
    /// The parts read from the database (all but History).
    pub const ALL: [DetailTab; 8] = [
        DetailTab::Overview,
        DetailTab::Sql,
        DetailTab::Plan,
        DetailTab::Waits,
        DetailTab::Stats,
        DetailTab::Cursors,
        DetailTab::Locks,
        DetailTab::Columns,
    ];

    /// The sections (as errors name them) the part reads.
    fn sections(self) -> &'static [&'static str] {
        match self {
            DetailTab::Overview => &[
                "Client",
                "Connection",
                "Temp space",
                "Process",
                "Work areas",
                "Long operations",
                "PL/SQL",
                "Time model",
                "Waits by event",
            ],
            DetailTab::Sql => &["SQL", "Binds"],
            DetailTab::Plan => &["Plan"],
            DetailTab::Waits => &["Wait history"],
            DetailTab::Stats => &["Statistics"],
            DetailTab::Cursors => &["Open cursors"],
            DetailTab::Locks => &["Locks"],
            DetailTab::Columns => &["Columns"],
            DetailTab::History => &[],
        }
    }
}

/// The sections of the header, read with every part.
const HEADER_SECTIONS: [&str; 3] = ["Session", "Blocking", "Transaction"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionDetail {
    pub session: Option<SessionRow>,
    /// Sessions waiting for this one: sid, serial#, user.
    pub blocks: Vec<(i64, i64, Option<String>)>,
    pub transaction: Option<Transaction>,
    pub overview: Option<SessionOverview>,
    pub sql_text: Option<String>,
    pub prev_sql_text: Option<String>,
    /// Captured bind values of the current and the previous SQL.
    pub binds: Vec<Bind>,
    pub prev_binds: Vec<Bind>,
    /// DBMS_XPLAN's report of the session's cursor, and its lines with the
    /// last execution's statistics if it collected them.
    pub plan: Vec<String>,
    pub plan_rows: Vec<pythia_ui::plan::PlanRow>,
    pub waits: Vec<WaitHistory>,
    pub stats: Vec<(String, f64)>,
    pub cursors: Vec<OpenCursor>,
    pub locks: Vec<LockRow>,
    /// The `V$SESSION` row: column, value.
    pub columns: Vec<(String, String)>,
    /// The parts read.
    pub loaded: Vec<DetailTab>,
    pub errors: Vec<SectionError>,
}

impl SessionDetail {
    /// Takes the header and the parts of a newer read, keeping the parts
    /// it didn't read. Once the session is gone, nothing of it is kept.
    pub fn merge(&mut self, mut newer: SessionDetail) {
        if newer.session.is_none() {
            *self = newer;
            return;
        }
        let reread: Vec<&str> = HEADER_SECTIONS
            .iter()
            .chain(newer.loaded.iter().flat_map(|p| p.sections()))
            .copied()
            .collect();
        self.errors.retain(|(name, _)| !reread.contains(name));
        self.errors.append(&mut newer.errors);
        self.session = newer.session;
        self.blocks = newer.blocks;
        self.transaction = newer.transaction;
        for part in newer.loaded {
            match part {
                DetailTab::Overview => self.overview = newer.overview.take(),
                DetailTab::Sql => {
                    self.sql_text = newer.sql_text.take();
                    self.prev_sql_text = newer.prev_sql_text.take();
                    self.binds = std::mem::take(&mut newer.binds);
                    self.prev_binds = std::mem::take(&mut newer.prev_binds);
                }
                DetailTab::Plan => {
                    self.plan = std::mem::take(&mut newer.plan);
                    self.plan_rows = std::mem::take(&mut newer.plan_rows);
                }
                DetailTab::Waits => self.waits = std::mem::take(&mut newer.waits),
                DetailTab::Stats => self.stats = std::mem::take(&mut newer.stats),
                DetailTab::Cursors => self.cursors = std::mem::take(&mut newer.cursors),
                DetailTab::Locks => self.locks = std::mem::take(&mut newer.locks),
                DetailTab::Columns => self.columns = std::mem::take(&mut newer.columns),
                DetailTab::History => {}
            }
            if !self.loaded.contains(&part) {
                self.loaded.push(part);
            }
        }
    }
}

/// What the Overview of session details shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionOverview {
    /// Who and where: name, value.
    pub client: Vec<(String, String)>,
    /// How it connected: name, value (`V$SESSION_CONNECT_INFO`).
    pub connection: Vec<(String, String)>,
    pub temp: Vec<TempUse>,
    pub process: Option<ProcessMemory>,
    /// Sorts, hash joins and the like running now.
    pub workareas: Vec<Workarea>,
    pub long_ops: Vec<LongOp>,
    /// The PL/SQL it runs: its entry point and the current subprogram.
    pub plsql: Vec<PlsqlUnit>,
    /// DB time and its parts, in seconds, largest first.
    pub time_model: Vec<(String, f64)>,
    /// Non-idle waits since logon, longest first.
    pub events: Vec<SessionEvent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TempUse {
    pub tablespace: String,
    /// SORT, HASH, LOB_DATA, …
    pub segment: String,
    pub bytes: f64,
}

/// The session's server process and its PGA, in bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessMemory {
    pub spid: String,
    pub pga_used: f64,
    pub pga_allocated: f64,
    pub pga_max: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workarea {
    pub operation: String,
    pub memory: f64,
    pub temp: f64,
    pub sql_id: Option<String>,
}

/// A PL/SQL unit a session runs, to open at its subprogram.
#[derive(Debug, Clone, PartialEq)]
pub struct PlsqlUnit {
    /// "Entry point" or "Running".
    pub role: &'static str,
    pub owner: String,
    pub name: String,
    /// As in ALL_OBJECTS, e.g. `PACKAGE`.
    pub object_type: String,
    pub subprogram: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEvent {
    pub event: String,
    pub wait_class: String,
    pub waits: i64,
    pub seconds: f64,
    pub average_ms: f64,
}

/// A captured bind value (`V$SQL_BIND_CAPTURE`).
#[derive(Debug, Clone, PartialEq)]
pub struct Bind {
    pub name: Option<String>,
    pub position: i64,
    pub data_type: String,
    pub value: Option<String>,
    pub captured: Option<String>,
}

/// Everything about a session, e.g. for the assistant.
pub fn session_detail(conn: &Connection, sid: i64, serial: i64) -> SessionDetail {
    session_details(conn, sid, serial, &DetailTab::ALL)
}

/// A session's header (its row, the sessions it blocks, its transaction)
/// and the `parts` asked for.
pub fn session_details(
    conn: &Connection,
    sid: i64,
    serial: i64,
    parts: &[DetailTab],
) -> SessionDetail {
    let mut d = SessionDetail::default();
    let mut errors = Vec::new();
    let mut section = |name: &'static str, r: Result<()>| {
        if let Err(e) = r {
            errors.push((name, explain_error(&e)));
        }
    };
    section(
        "Session",
        (|| {
            let sql = format!("{SESSION_SELECT} where s.sid = :1 and s.serial# = :2");
            d.session = with_objects(&sql, |sql| {
                with_sql_text(sql, |sql| query(conn, sql, &[&sid, &serial], session_row))
            })?
            .into_iter()
            .next();
            Ok(())
        })(),
    );
    // The other views know sessions by SID alone: once the session has
    // ended, its SID may belong to another one.
    let Some(session) = d.session.clone() else {
        d.errors = errors;
        return d;
    };
    section(
        "Blocking",
        query(
            conn,
            "select sid, serial#, username from v$session where blocking_session = :1 order by sid",
            &[&sid],
            |r| Ok((get(r, 0)?, get(r, 1)?, get(r, 2)?)),
        )
        .map(|b| d.blocks = b),
    );
    section(
        "Transaction",
        transaction(conn, sid, serial).map(|t| d.transaction = t),
    );
    for &part in parts {
        match part {
            DetailTab::Overview => {
                d.overview = Some(session_overview(conn, sid, serial, &mut section));
            }
            DetailTab::Sql => {
                let full_text = |id: &str| -> Result<Option<String>> {
                    Ok(query(
                        conn,
                        "select sql_fulltext from v$sql where sql_id = :1 and rownum = 1",
                        &[&id],
                        |r| get::<Option<String>>(r, 0),
                    )?
                    .into_iter()
                    .next()
                    .flatten())
                };
                section(
                    "SQL",
                    (|| {
                        if let Some(id) = &session.sql_id {
                            d.sql_text = full_text(id)?;
                        }
                        if let Some(id) = &session.prev_sql_id {
                            d.prev_sql_text = full_text(id)?;
                        }
                        Ok(())
                    })(),
                );
                section(
                    "Binds",
                    (|| {
                        if let Some(id) = &session.sql_id {
                            d.binds = binds(conn, id, session.sql_child)?;
                        }
                        if let Some(id) = &session.prev_sql_id {
                            d.prev_binds = binds(conn, id, session.prev_child)?;
                        }
                        Ok(())
                    })(),
                );
            }
            DetailTab::Plan => {
                // The session's own child cursor, with the binds it was
                // optimized for.
                let cursor = match &session.sql_id {
                    Some(id) => Some((id, session.sql_child)),
                    None => session.prev_sql_id.as_ref().map(|id| (id, session.prev_child)),
                };
                if let Some((id, child)) = cursor {
                    section(
                        "Plan",
                        cursor_plan(conn, id, child).map(|(text, rows)| {
                            d.plan = text;
                            d.plan_rows = rows;
                        }),
                    );
                }
            }
            DetailTab::Waits => section(
                "Wait history",
                query(
                    conn,
                    "select seq#, event, wait_time_micro / 1000, \
                            p1text || '=' || p1 || ' ' || p2text || '=' || p2 || ' ' || p3text || '=' || p3 \
                       from v$session_wait_history where sid = :1 order by seq#",
                    &[&sid],
                    |r| {
                        Ok(WaitHistory {
                            seq: get(r, 0)?,
                            event: get::<Option<String>>(r, 1)?.unwrap_or_default(),
                            wait_ms: get::<Option<f64>>(r, 2)?.unwrap_or(0.0),
                            params: get::<Option<String>>(r, 3)?.unwrap_or_default(),
                        })
                    },
                )
                .map(|w| d.waits = w),
            ),
            DetailTab::Stats => section(
                "Statistics",
                query(
                    conn,
                    "select n.name, s.value from v$sesstat s join v$statname n on n.statistic# = s.statistic# \
                      where s.sid = :1 and s.value <> 0 order by n.name",
                    &[&sid],
                    |r| Ok((get(r, 0)?, get(r, 1)?)),
                )
                .map(|s| d.stats = s),
            ),
            DetailTab::Cursors => section(
                "Open cursors",
                query(
                    conn,
                    // V$OPEN_CURSOR has the first 60 characters of the
                    // statement, V$SQL the first 1000 and, for longer ones,
                    // the whole text.
                    "select oc.sql_id, oc.cursor_type, nvl(q.sql_text, oc.sql_text), \
                            case when dbms_lob.getlength(q.sql_fulltext) > length(q.sql_text) \
                                 then q.sql_fulltext end \
                       from v$open_cursor oc \
                       left join v$sql q on q.address = oc.address and q.child_address = oc.child_address \
                      where oc.sid = :1 \
                      order by 3",
                    &[&sid],
                    |r| {
                        let text: Option<String> = get(r, 2)?;
                        let full: Option<String> = get(r, 3)?;
                        Ok(OpenCursor {
                            sql_id: get(r, 0)?,
                            cursor_type: get(r, 1)?,
                            text: full.or(text).unwrap_or_default(),
                        })
                    },
                )
                .map(|c| d.cursors = c),
            ),
            DetailTab::Locks => section("Locks", locks(conn, sid).map(|l| d.locks = l)),
            DetailTab::Columns => section(
                "Columns",
                session_columns(conn, sid, serial).map(|c| d.columns = c),
            ),
            DetailTab::History => {}
        }
    }
    d.loaded = parts.to_vec();
    d.errors = errors;
    d
}

/// DBMS_XPLAN's report of a child cursor and its plan lines. With row
/// source statistics (GATHER_PLAN_STATISTICS or STATISTICS_LEVEL = ALL)
/// both have the last execution's actual rows and times.
fn cursor_plan(
    conn: &Connection,
    sql_id: &str,
    child: Option<i64>,
) -> Result<(Vec<String>, Vec<pythia_ui::plan::PlanRow>)> {
    let rows = match child {
        Some(child) => match pythia_db::cursor_plan(conn, sql_id, child)? {
            pythia_db::MetaResult::Table { columns, rows } => {
                pythia_ui::plan::cursor_rows(&columns, &rows)
            }
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    let format = if pythia_ui::plan::has_actuals(&rows) {
        "ALLSTATS LAST +PEEKED_BINDS"
    } else {
        "TYPICAL +PEEKED_BINDS"
    };
    let text = query(
        conn,
        "select plan_table_output from table(dbms_xplan.display_cursor(:1, :2, :3))",
        &[&sql_id, &child, &format],
        |r| Ok(get::<Option<String>>(r, 0)?.unwrap_or_default()),
    )?;
    Ok((text, rows))
}

/// A statement in the shared pool, for SQL details.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SqlDetail {
    pub sql_id: String,
    pub text: Option<String>,
    pub stats: Option<SqlStats>,
    pub children: Vec<ChildCursor>,
    /// The child cursor the plan, binds and sharing are of.
    pub child: Option<i64>,
    /// DBMS_XPLAN's report of the child, and its lines with the last
    /// execution's statistics if it collected them.
    pub plan: Vec<String>,
    pub plan_rows: Vec<pythia_ui::plan::PlanRow>,
    pub binds: Vec<Bind>,
    /// Why the child isn't shared with the others (`V$SQL_SHARED_CURSOR`).
    pub not_shared: Vec<String>,
    /// Sessions running it now: sid, serial#, user, status.
    pub sessions: Vec<(i64, i64, Option<String>, String)>,
    pub errors: Vec<SectionError>,
}

/// Totals since the statement was loaded (`V$SQLSTATS`), times in seconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SqlStats {
    pub executions: f64,
    pub elapsed: f64,
    pub cpu: f64,
    pub buffer_gets: f64,
    pub disk_reads: f64,
    pub rows: f64,
    pub parse_calls: f64,
    pub fetches: f64,
    pub sorts: f64,
    pub application_wait: f64,
    pub concurrency_wait: f64,
    pub user_io_wait: f64,
    pub plsql: f64,
    pub last_active: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChildCursor {
    pub child: i64,
    pub plan_hash: i64,
    pub executions: f64,
    pub elapsed: f64,
    pub buffer_gets: f64,
    pub rows: f64,
    pub parsing_schema: Option<String>,
    pub bind_sensitive: bool,
    pub bind_aware: bool,
    pub profile: Option<String>,
    pub baseline: Option<String>,
    pub patch: Option<String>,
    pub last_active: Option<String>,
}

/// Everything about a statement in the shared pool; `child` picks the
/// child cursor whose plan, binds and sharing are shown (else the most
/// executed).
pub fn sql_detail(conn: &Connection, sql_id: &str, child: Option<i64>) -> SqlDetail {
    let mut d = SqlDetail {
        sql_id: sql_id.to_owned(),
        ..SqlDetail::default()
    };
    let mut errors = Vec::new();
    let mut section = |name: &'static str, r: Result<()>| {
        if let Err(e) = r {
            errors.push((name, explain_error(&e)));
        }
    };
    let f = |r: &oracle::Row, i| -> Result<f64> { Ok(get::<Option<f64>>(r, i)?.unwrap_or(0.0)) };
    section(
        "Statistics",
        query(
            conn,
            "select sql_fulltext, executions, elapsed_time / 1e6, cpu_time / 1e6, buffer_gets, \
                    disk_reads, rows_processed, parse_calls, fetches, sorts, \
                    application_wait_time / 1e6, concurrency_wait_time / 1e6, \
                    user_io_wait_time / 1e6, plsql_exec_time / 1e6, \
                    to_char(last_active_time, 'YYYY-MM-DD HH24:MI:SS') \
               from v$sqlstats where sql_id = :1",
            &[&sql_id],
            |r| {
                Ok((
                    get::<Option<String>>(r, 0)?,
                    SqlStats {
                        executions: f(r, 1)?,
                        elapsed: f(r, 2)?,
                        cpu: f(r, 3)?,
                        buffer_gets: f(r, 4)?,
                        disk_reads: f(r, 5)?,
                        rows: f(r, 6)?,
                        parse_calls: f(r, 7)?,
                        fetches: f(r, 8)?,
                        sorts: f(r, 9)?,
                        application_wait: f(r, 10)?,
                        concurrency_wait: f(r, 11)?,
                        user_io_wait: f(r, 12)?,
                        plsql: f(r, 13)?,
                        last_active: get(r, 14)?,
                    },
                ))
            },
        )
        .map(|rows| {
            if let Some((text, stats)) = rows.into_iter().next() {
                d.text = text;
                d.stats = Some(stats);
            }
        }),
    );
    section(
        "Child cursors",
        query(
            conn,
            "select child_number, plan_hash_value, executions, elapsed_time / 1e6, buffer_gets, \
                    rows_processed, parsing_schema_name, is_bind_sensitive, is_bind_aware, \
                    sql_profile, sql_plan_baseline, sql_patch, \
                    to_char(last_active_time, 'YYYY-MM-DD HH24:MI:SS') \
               from v$sql where sql_id = :1 order by child_number",
            &[&sql_id],
            |r| {
                Ok(ChildCursor {
                    child: get(r, 0)?,
                    plan_hash: get::<Option<i64>>(r, 1)?.unwrap_or(0),
                    executions: f(r, 2)?,
                    elapsed: f(r, 3)?,
                    buffer_gets: f(r, 4)?,
                    rows: f(r, 5)?,
                    parsing_schema: get(r, 6)?,
                    bind_sensitive: get::<Option<String>>(r, 7)?.as_deref() == Some("Y"),
                    bind_aware: get::<Option<String>>(r, 8)?.as_deref() == Some("Y"),
                    profile: get(r, 9)?,
                    baseline: get(r, 10)?,
                    patch: get(r, 11)?,
                    last_active: get(r, 12)?,
                })
            },
        )
        .map(|c| d.children = c),
    );
    if d.text.is_none() {
        // Aged out of V$SQLSTATS but a child is still there, or the other
        // way round: the text from V$SQL.
        section(
            "Statement",
            query(
                conn,
                "select sql_fulltext from v$sql where sql_id = :1 and rownum = 1",
                &[&sql_id],
                |r| get::<Option<String>>(r, 0),
            )
            .map(|t| d.text = t.into_iter().next().flatten()),
        );
    }
    d.child = child
        .filter(|c| d.children.iter().any(|x| x.child == *c))
        .or_else(|| {
            d.children
                .iter()
                .max_by(|a, b| a.executions.total_cmp(&b.executions))
                .map(|c| c.child)
        });
    if let Some(child) = d.child {
        section(
            "Plan",
            cursor_plan(conn, sql_id, Some(child)).map(|(text, rows)| {
                d.plan = text;
                d.plan_rows = rows;
            }),
        );
        section(
            "Binds",
            binds(conn, sql_id, Some(child)).map(|b| d.binds = b),
        );
        if d.children.len() > 1 {
            section(
                "Sharing",
                not_shared(conn, sql_id, child).map(|n| d.not_shared = n),
            );
        }
    }
    section(
        "Sessions",
        query(
            conn,
            "select sid, serial#, username, status from v$session where sql_id = :1 order by sid",
            &[&sql_id],
            |r| Ok((get(r, 0)?, get(r, 1)?, get(r, 2)?, get(r, 3)?)),
        )
        .map(|s| d.sessions = s),
    );
    d.errors = errors;
    d
}

/// Why a child cursor wasn't shared: the `V$SQL_SHARED_CURSOR` flags that
/// are `Y`, and the reasons Oracle gives.
fn not_shared(conn: &Connection, sql_id: &str, child: i64) -> Result<Vec<String>> {
    let rows = conn.query(
        "select * from v$sql_shared_cursor where sql_id = :1 and child_number = :2",
        &[&sql_id, &child],
    )?;
    let names: Vec<String> = rows
        .column_info()
        .iter()
        .map(|c| c.name().to_owned())
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        for (name, value) in names.iter().zip(row.sql_values()) {
            let value = if value.is_null().unwrap_or(true) {
                continue;
            } else {
                value.get::<String>().unwrap_or_default()
            };
            if name == "REASON" {
                // XML: <ChildNode>…<reason>Bind mismatch(…)</reason>…
                let mut rest = value.as_str();
                while let Some(start) = rest.find("<reason>") {
                    let after = &rest[start + "<reason>".len()..];
                    let Some(end) = after.find("</reason>") else {
                        break;
                    };
                    let reason = after[..end].trim().to_owned();
                    if !reason.is_empty() && !out.contains(&reason) {
                        out.push(reason);
                    }
                    rest = &after[end..];
                }
            } else if value == "Y" {
                out.push(name.to_lowercase().replace('_', " "));
            }
        }
    }
    // A flag that a reason says in more detail ("bind mismatch",
    // "Bind mismatch(8)") once.
    let detailed: Vec<String> = out.iter().map(|r| r.to_lowercase()).collect();
    out.retain(|r| {
        let flag = r.to_lowercase();
        !detailed.iter().any(|d| *d != flag && d.starts_with(&flag))
    });
    Ok(out)
}

/// The captured binds of a cursor (any child if it isn't known).
fn binds(conn: &Connection, sql_id: &str, child: Option<i64>) -> Result<Vec<Bind>> {
    query(
        conn,
        "select name, position, datatype_string, value_string, \
                to_char(last_captured, 'YYYY-MM-DD HH24:MI:SS') \
           from v$sql_bind_capture \
          where sql_id = :1 and child_number = nvl(:2, child_number) \
          order by child_number, position",
        &[&sql_id, &child],
        |r| {
            Ok(Bind {
                name: get(r, 0)?,
                position: get(r, 1)?,
                data_type: get::<Option<String>>(r, 2)?.unwrap_or_default(),
                value: get(r, 3)?,
                captured: get(r, 4)?,
            })
        },
    )
}

/// The `V$SESSION` row as column, value.
fn session_columns(conn: &Connection, sid: i64, serial: i64) -> Result<Vec<(String, String)>> {
    let rows = conn.query(
        "select * from v$session where sid = :1 and serial# = :2",
        &[&sid, &serial],
    )?;
    let names: Vec<String> = rows
        .column_info()
        .iter()
        .map(|c| c.name().to_owned())
        .collect();
    let Some(row) = rows.into_iter().next() else {
        return Ok(Vec::new());
    };
    let row = row?;
    let values = row.sql_values().iter().map(|v| {
        if v.is_null().unwrap_or(true) {
            String::new()
        } else {
            v.get::<String>().unwrap_or_else(|_| "…".into())
        }
    });
    Ok(names.into_iter().zip(values).collect())
}

/// The Overview of session details; each of its sections may fail alone.
fn session_overview(
    conn: &Connection,
    sid: i64,
    serial: i64,
    section: &mut impl FnMut(&'static str, Result<()>),
) -> SessionOverview {
    let mut o = SessionOverview::default();
    let both: [&dyn ToSql; 2] = [&sid, &serial];
    let mut plsql_ids: Vec<(&'static str, i64, i64)> = Vec::new();
    section(
        "Client",
        (|| {
            let rows = query(
                conn,
                "select osuser, machine, terminal, process, to_char(port), program, module, action, \
                        client_identifier, client_info, service_name, server, schemaname, \
                        to_char(sql_exec_start, 'YYYY-MM-DD HH24:MI:SS'), sql_trace, \
                        plsql_entry_object_id, plsql_entry_subprogram_id, \
                        plsql_object_id, plsql_subprogram_id \
                   from v$session where sid = :1 and serial# = :2",
                &both,
                |r| {
                    let mut text = Vec::new();
                    for i in 0..15 {
                        text.push(get::<Option<String>>(r, i)?);
                    }
                    let ids: Vec<Option<i64>> =
                        (15..19).map(|i| get(r, i)).collect::<Result<_>>()?;
                    Ok((text, ids))
                },
            )?;
            let Some((text, ids)) = rows.into_iter().next() else {
                return Ok(());
            };
            const NAMES: [&str; 15] = [
                "OS user",
                "Machine",
                "Terminal",
                "Client process",
                "Client port",
                "Program",
                "Module",
                "Action",
                "Client identifier",
                "Client info",
                "Service",
                "Server",
                "Current schema",
                "SQL running since",
                "SQL trace",
            ];
            o.client = NAMES
                .iter()
                .zip(text)
                .filter_map(|(name, value)| {
                    value
                        .filter(|v| !v.is_empty() && v != "0")
                        .map(|v| ((*name).to_owned(), v))
                })
                .collect();
            if let (Some(object), Some(sub)) = (ids[0], ids[1]) {
                plsql_ids.push(("Entry point", object, sub));
            }
            if let (Some(object), Some(sub)) = (ids[2], ids[3]) {
                plsql_ids.push(("Running", object, sub));
            }
            Ok(())
        })(),
    );
    section(
        "Connection",
        (|| {
            let rows = query(
                conn,
                "select authentication_type, client_driver, client_version, client_charset, \
                        network_service_banner \
                   from v$session_connect_info where sid = :1 and serial# = :2",
                &both,
                |r| {
                    (0..5)
                        .map(|i| get::<Option<String>>(r, i))
                        .collect::<Result<Vec<_>>>()
                },
            )?;
            let first = |i: usize| {
                rows.iter()
                    .find_map(|r| r[i].clone())
                    .filter(|v| !v.is_empty())
            };
            for (i, name) in [
                "Authentication",
                "Client driver",
                "Client version",
                "Client character set",
            ]
            .into_iter()
            .enumerate()
            {
                if let Some(v) = first(i) {
                    o.connection.push((name.to_owned(), v));
                }
            }
            // An encryption or checksumming adapter in use names its
            // algorithm.
            let secure: Vec<String> = rows
                .iter()
                .filter_map(|r| r[4].clone())
                .filter(|b| {
                    let b = b.to_lowercase();
                    (b.contains("encryption") || b.contains("crypto")) && b.contains("adapter")
                })
                .collect();
            o.connection.push((
                "Network encryption".to_owned(),
                if secure.is_empty() {
                    "none reported".to_owned()
                } else {
                    secure.join("; ")
                },
            ));
            Ok(())
        })(),
    );
    section(
        "Temp space",
        query(
            conn,
            "select u.tablespace, u.segtype, u.blocks * t.block_size \
               from v$tempseg_usage u \
               join v$session s on s.saddr = u.session_addr \
               join dba_tablespaces t on t.tablespace_name = u.tablespace \
              where s.sid = :1 and s.serial# = :2 \
              order by 3 desc",
            &both,
            |r| {
                Ok(TempUse {
                    tablespace: get(r, 0)?,
                    segment: get::<Option<String>>(r, 1)?.unwrap_or_default(),
                    bytes: get::<Option<f64>>(r, 2)?.unwrap_or(0.0),
                })
            },
        )
        .map(|t| o.temp = t),
    );
    section(
        "Process",
        query(
            conn,
            "select p.spid, p.pga_used_mem, p.pga_alloc_mem, p.pga_max_mem \
               from v$process p join v$session s on s.paddr = p.addr \
              where s.sid = :1 and s.serial# = :2",
            &both,
            |r| {
                Ok(ProcessMemory {
                    spid: get::<Option<String>>(r, 0)?.unwrap_or_default(),
                    pga_used: get::<Option<f64>>(r, 1)?.unwrap_or(0.0),
                    pga_allocated: get::<Option<f64>>(r, 2)?.unwrap_or(0.0),
                    pga_max: get::<Option<f64>>(r, 3)?.unwrap_or(0.0),
                })
            },
        )
        .map(|p| o.process = p.into_iter().next()),
    );
    section(
        "Work areas",
        query(
            conn,
            "select operation_type, actual_mem_used, nvl(tempseg_size, 0), sql_id \
               from v$sql_workarea_active where sid = :1 order by actual_mem_used desc",
            &[&sid],
            |r| {
                Ok(Workarea {
                    operation: get::<Option<String>>(r, 0)?.unwrap_or_default(),
                    memory: get::<Option<f64>>(r, 1)?.unwrap_or(0.0),
                    temp: get::<Option<f64>>(r, 2)?.unwrap_or(0.0),
                    sql_id: get(r, 3)?,
                })
            },
        )
        .map(|w| o.workareas = w),
    );
    section(
        "Long operations",
        long_ops_of(conn, Some((sid, serial))).map(|l| o.long_ops = l),
    );
    section(
        "PL/SQL",
        (|| {
            for (role, object, subprogram) in plsql_ids {
                let sql = "select owner, object_name, procedure_name, object_type \
                             from {procedures} where object_id = :1 and subprogram_id = :2";
                let unit = with_procedures(sql, |sql| {
                    query(conn, sql, &[&object, &subprogram], |r| {
                        Ok(PlsqlUnit {
                            role,
                            owner: get(r, 0)?,
                            name: get(r, 1)?,
                            subprogram: get(r, 2)?,
                            object_type: get(r, 3)?,
                        })
                    })
                })?;
                o.plsql.extend(unit.into_iter().next());
            }
            Ok(())
        })(),
    );
    section(
        "Time model",
        query(
            conn,
            "select stat_name, value / 1e6 from v$sess_time_model \
              where sid = :1 and value > 0 order by value desc",
            &[&sid],
            |r| Ok((get(r, 0)?, get(r, 1)?)),
        )
        .map(|t| o.time_model = t),
    );
    section(
        "Waits by event",
        query(
            conn,
            &compat::first_rows(
                "select event, wait_class, total_waits, time_waited_micro / 1e6, \
                        time_waited_micro / 1000 / nullif(total_waits, 0) \
                   from v$session_event \
                  where sid = :1 and wait_class <> 'Idle' \
                  order by time_waited_micro desc",
                15,
            ),
            &[&sid],
            |r| {
                Ok(SessionEvent {
                    event: get(r, 0)?,
                    wait_class: get::<Option<String>>(r, 1)?.unwrap_or_default(),
                    waits: get::<Option<i64>>(r, 2)?.unwrap_or(0),
                    seconds: get::<Option<f64>>(r, 3)?.unwrap_or(0.0),
                    average_ms: get::<Option<f64>>(r, 4)?.unwrap_or(0.0),
                })
            },
        )
        .map(|e| o.events = e),
    );
    o
}

fn lock_mode(m: i64) -> String {
    match m {
        0 => "None",
        1 => "Null",
        2 => "Row-S (SS)",
        3 => "Row-X (SX)",
        4 => "Share",
        5 => "S/Row-X (SSX)",
        6 => "Exclusive",
        _ => return m.to_string(),
    }
    .to_owned()
}

pub fn locks(conn: &Connection, sid: i64) -> Result<Vec<LockRow>> {
    let sql = "select l.type, l.id1, l.id2, l.lmode, l.request, l.ctime, l.block, \
                      (select o.owner || '.' || o.object_name from {objects} o \
                        where o.object_id = l.id1 and l.type = 'TM') \
                 from v$lock l where l.sid = :1 order by l.type, l.id1";
    with_objects(sql, |sql| locks_with(conn, sql, sid))
}

fn locks_with(conn: &Connection, sql: &str, sid: i64) -> Result<Vec<LockRow>> {
    query(conn, sql, &[&sid], |r| {
        Ok(LockRow {
            lock_type: get(r, 0)?,
            id1: get(r, 1)?,
            id2: get(r, 2)?,
            mode_held: lock_mode(get(r, 3)?),
            mode_requested: lock_mode(get(r, 4)?),
            seconds: get(r, 5)?,
            blocking: get::<i64>(r, 6)? > 0,
            object: get(r, 7)?,
        })
    })
}

// ----- session control ---------------------------------------------------

fn session_handle(sid: i64, serial: i64) -> String {
    // Both are numbers, so formatting them into the statement is safe.
    format!("'{sid},{serial}'")
}

pub fn kill_session(conn: &Connection, sid: i64, serial: i64, immediate: bool) -> Result<()> {
    // A session busy in a call is only marked for kill (ORA-00031) and goes
    // when the call returns. The driver takes ORA-00031 on any call for
    // "this connection was killed" and would drop the monitor's own
    // connection, so it is caught here.
    let sql = format!(
        "begin\n  execute immediate 'alter system kill session {}{}';\n\
         exception\n  when others then\n    if sqlcode != -31 then raise; end if;\nend;",
        session_handle(sid, serial).replace('\'', "''"),
        if immediate { " immediate" } else { "" }
    );
    conn.execute(&sql, &[])?;
    Ok(())
}

/// Disconnects the session once its open transaction has ended (or right
/// away without one).
pub fn disconnect_session(conn: &Connection, sid: i64, serial: i64) -> Result<()> {
    // As with kill: a session busy in a call is only marked (ORA-00031).
    let sql = format!(
        "begin\n  execute immediate 'alter system disconnect session {} post_transaction';\n\
         exception\n  when others then\n    if sqlcode != -31 then raise; end if;\nend;",
        session_handle(sid, serial).replace('\'', "''"),
    );
    conn.execute(&sql, &[])?;
    Ok(())
}

/// A session's open transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct Transaction {
    pub start: String,
    pub undo_blocks: i64,
    /// Undo records: what a rollback has to undo.
    pub undo_records: i64,
    pub status: String,
}

/// The session's open transaction, if it has one.
pub fn transaction(conn: &Connection, sid: i64, serial: i64) -> Result<Option<Transaction>> {
    Ok(query(
        conn,
        "select to_char(t.start_date, 'YYYY-MM-DD HH24:MI:SS'), t.used_ublk, t.used_urec, t.status \
           from v$transaction t join v$session s on s.taddr = t.addr \
          where s.sid = :1 and s.serial# = :2",
        &[&sid, &serial],
        |r| {
            Ok(Transaction {
                start: get::<Option<String>>(r, 0)?.unwrap_or_default(),
                undo_blocks: get(r, 1)?,
                undo_records: get(r, 2)?,
                status: get(r, 3)?,
            })
        },
    )?
    .into_iter()
    .next())
}

/// Cancels the session's current call (Oracle 18c+).
pub fn cancel_sql(conn: &Connection, sid: i64, serial: i64) -> Result<()> {
    conn.execute(
        &format!("alter system cancel sql {}", session_handle(sid, serial)),
        &[],
    )?;
    Ok(())
}

// ----- tracing -----------------------------------------------------------

pub fn trace_enable(
    conn: &Connection,
    sid: i64,
    serial: i64,
    waits: bool,
    binds: bool,
) -> Result<()> {
    conn.execute(
        "begin dbms_monitor.session_trace_enable(session_id => :1, serial_num => :2, \
           waits => :3 = 1, binds => :4 = 1, plan_stat => 'ALL_EXECUTIONS'); end;",
        &[&sid, &serial, &i32::from(waits), &i32::from(binds)],
    )?;
    Ok(())
}

pub fn trace_disable(conn: &Connection, sid: i64, serial: i64) -> Result<()> {
    conn.execute(
        "begin dbms_monitor.session_trace_disable(session_id => :1, serial_num => :2); end;",
        &[&sid, &serial],
    )?;
    Ok(())
}

/// Enables 10046 tracing (waits and binds) for the calling session itself.
pub fn trace_self(conn: &Connection, on: bool) -> Result<()> {
    let sql = if on {
        "begin dbms_monitor.session_trace_enable(waits => true, binds => true, plan_stat => 'ALL_EXECUTIONS'); end;"
    } else {
        "begin dbms_monitor.session_trace_disable; end;"
    };
    conn.execute(sql, &[])?;
    Ok(())
}

/// Trace file of a session (from V$PROCESS), as (directory, file name).
pub fn session_trace_file(conn: &Connection, sid: i64) -> Result<Option<(String, String)>> {
    let path: Option<String> = query(
        conn,
        "select p.tracefile from v$session s join v$process p on p.addr = s.paddr where s.sid = :1",
        &[&sid],
        |r| get(r, 0),
    )?
    .into_iter()
    .next()
    .flatten();
    Ok(path.and_then(|p| {
        let (dir, file) = p.rsplit_once('/')?;
        Some((dir.to_owned(), file.to_owned()))
    }))
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraceFile {
    pub adr_home: String,
    pub name: String,
    pub changed: String,
}

pub fn trace_files(conn: &Connection, filter: &str, limit: usize) -> Result<Vec<TraceFile>> {
    let pattern = format!("%{}%", filter.trim());
    query(
        conn,
        &compat::first_rows(
            "select adr_home, trace_filename, to_char(change_time, 'YYYY-MM-DD HH24:MI:SS') \
               from v$diag_trace_file \
              where trace_filename like :1 and trace_filename like '%.trc' \
              order by change_time desc",
            limit,
        ),
        &[&pattern],
        |r| {
            Ok(TraceFile {
                adr_home: get(r, 0)?,
                name: get(r, 1)?,
                changed: get::<Option<String>>(r, 2)?.unwrap_or_default(),
            })
        },
    )
}

/// Reads a trace file through V$DIAG_TRACE_FILE_CONTENTS (Oracle 12.2+).
///
/// `adr_home` may also be the directory of the trace file (as returned by
/// [`session_trace_file`]); its `/trace` suffix is stripped.
pub fn read_trace_file(conn: &Connection, adr_home: &str, name: &str) -> Result<String> {
    let home = adr_home.strip_suffix("/trace").unwrap_or(adr_home);
    let rows = conn.query_as::<Option<String>>(
        "select payload from v$diag_trace_file_contents \
          where adr_home = :1 and trace_filename = :2 order by line_number",
        &[&home, &name],
    )?;
    let mut text = String::new();
    for line in rows {
        let line = line?.unwrap_or_default();
        text.push_str(&line);
        if !line.ends_with('\n') {
            text.push('\n');
        }
    }
    Ok(text)
}

// ----- AWR / ASH ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct AwrSnapshot {
    pub snap_id: i64,
    pub dbid: i64,
    pub instance: i64,
    pub begin: String,
    pub end: String,
}

/// The `V$DATABASE` column with the AWR id of the current container
/// (`CON_DBID`, 12.1+), or of the database before containers.
fn dbid_column(conn: &Connection) -> Result<&'static str> {
    Ok(if compat::at_least(conn, 12, 1)? {
        "con_dbid"
    } else {
        "dbid"
    })
}

pub fn awr_snapshots(conn: &Connection, limit: usize) -> Result<Vec<AwrSnapshot>> {
    let dbid = dbid_column(conn)?;
    query(
        conn,
        &compat::first_rows(
            &format!(
                "select snap_id, dbid, instance_number, \
                        to_char(begin_interval_time, 'YYYY-MM-DD HH24:MI'), \
                        to_char(end_interval_time, 'YYYY-MM-DD HH24:MI') \
                   from dba_hist_snapshot \
                  where dbid = (select {dbid} from v$database) \
                  order by snap_id desc"
            ),
            limit,
        ),
        &[],
        |r| {
            Ok(AwrSnapshot {
                snap_id: get(r, 0)?,
                dbid: get(r, 1)?,
                instance: get(r, 2)?,
                begin: get(r, 3)?,
                end: get(r, 4)?,
            })
        },
    )
}

/// Creates an AWR snapshot and returns its id.
pub fn awr_create_snapshot(conn: &Connection) -> Result<i64> {
    let mut stmt = conn
        .statement("begin :1 := dbms_workload_repository.create_snapshot(); end;")
        .build()?;
    stmt.execute(&[&OracleType::Number(0, 0)])?;
    Ok(stmt.bind_value(1)?)
}

fn collect_lines(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> Result<String> {
    let mut out = String::new();
    for line in conn.query_as::<Option<String>>(sql, params)? {
        out.push_str(&line?.unwrap_or_default());
        out.push('\n');
    }
    Ok(out)
}

/// AWR report between two snapshots, as text or HTML.
pub fn awr_report(
    conn: &Connection,
    dbid: i64,
    instance: i64,
    begin: i64,
    end: i64,
    html: bool,
) -> Result<String> {
    let func = if html {
        "awr_report_html"
    } else {
        "awr_report_text"
    };
    collect_lines(
        conn,
        &format!("select output from table(dbms_workload_repository.{func}(:1, :2, :3, :4))"),
        &[&dbid, &instance, &begin, &end],
    )
}

/// ASH report for the last `minutes` minutes, as text or HTML.
pub fn ash_report(conn: &Connection, minutes: i64, html: bool) -> Result<String> {
    ash_report_for(conn, None, minutes, html)
}

/// An ASH report of the last `minutes`, of one session or statement or of
/// everything.
pub fn ash_report_for(
    conn: &Connection,
    scope: Option<&Scope>,
    minutes: i64,
    html: bool,
) -> Result<String> {
    let func = if html {
        "ash_report_html"
    } else {
        "ash_report_text"
    };
    let dbid = dbid_column(conn)?;
    let (sid, sql_id): (Option<i64>, Option<&str>) = match scope {
        Some(Scope::Session { sid, .. }) => (Some(*sid), None),
        Some(Scope::Sql(id)) => (None, Some(id)),
        None => (None, None),
    };
    collect_lines(
        conn,
        &format!(
            "select output from table(dbms_workload_repository.{func}( \
               l_dbid => (select {dbid} from v$database), \
               l_inst_num => (select instance_number from v$instance), \
               l_btime => sysdate - :1 / 1440, l_etime => sysdate, \
               l_sid => :2, l_sql_id => :3))"
        ),
        &[&minutes, &sid, &sql_id],
    )
}

// ----- history (management packs) -------------------------------------

/// What activity, plan history and monitored executions are about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    Session { sid: i64, serial: i64 },
    Sql(String),
}

impl Scope {
    /// The `V$ACTIVE_SESSION_HISTORY` / `V$SQL_MONITOR` condition, with
    /// its binds.
    fn filter(&self, sid_col: &str, serial_col: &str) -> (String, Vec<Box<dyn ToSql>>) {
        match self {
            Scope::Session { sid, serial } => (
                format!("{sid_col} = :s1 and {serial_col} = :s2"),
                vec![Box::new(*sid), Box::new(*serial)],
            ),
            Scope::Sql(id) => ("sql_id = :s1".to_owned(), vec![Box::new(id.clone())]),
        }
    }
}

/// A session's or statement's activity from Active Session History
/// (Diagnostics Pack), in samples (about one per second active).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AshActivity {
    /// Per minute (`HH24:MI`) and wait class (`CPU` on CPU): samples.
    pub timeline: Vec<(String, String, i64)>,
    pub events: Vec<(String, i64)>,
    /// The statements a session ran.
    pub sql: Vec<(String, i64)>,
    /// The sessions that ran a statement: sid, serial#, samples.
    pub sessions: Vec<(i64, i64, i64)>,
    /// Objects waited on (`CURRENT_OBJ#`), `OWNER.NAME`.
    pub objects: Vec<(String, i64)>,
    /// Who blocked it: sid, serial#, samples.
    pub blockers: Vec<(i64, Option<i64>, i64)>,
    pub samples: i64,
}

/// Activity of the last `minutes` from `V$ACTIVE_SESSION_HISTORY`.
pub fn ash_activity(conn: &Connection, scope: &Scope, minutes: i64) -> Result<AshActivity> {
    let (filter, binds) = scope.filter("session_id", "session_serial#");
    // SAMPLE_TIME is in the server's time without a zone: compared with
    // SYSTIMESTAMP, it would take the session's time zone (the client's).
    let filter = format!(
        "{filter} and sample_time > cast(systimestamp as timestamp) - numtodsinterval(:m, 'MINUTE')"
    );
    let mut params: Vec<&dyn ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    params.push(&minutes);
    let ash = "v$active_session_history";
    let mut a = AshActivity {
        timeline: query(
            conn,
            &format!(
                "select to_char(trunc(sample_time, 'MI'), 'HH24:MI'), nvl(wait_class, 'CPU'), count(*) \
                   from {ash} where {filter} \
                  group by trunc(sample_time, 'MI'), nvl(wait_class, 'CPU') order by 1"
            ),
            &params,
            |r| Ok((get(r, 0)?, get(r, 1)?, get(r, 2)?)),
        )?,
        ..AshActivity::default()
    };
    a.samples = a.timeline.iter().map(|t| t.2).sum();
    let top = |select: &str, group: &str| -> String {
        compat::first_rows(
            &format!(
                "select {select}, count(*) from {ash} where {filter} group by {group} order by count(*) desc"
            ),
            10,
        )
    };
    a.events = query(
        conn,
        &top("nvl(event, 'ON CPU')", "nvl(event, 'ON CPU')"),
        &params,
        |r| Ok((get(r, 0)?, get(r, 1)?)),
    )?;
    match scope {
        Scope::Session { .. } => {
            a.sql = query(
                conn,
                &top("sql_id", "sql_id").replace(" group by", " and sql_id is not null group by"),
                &params,
                |r| Ok((get(r, 0)?, get(r, 1)?)),
            )?;
        }
        Scope::Sql(_) => {
            a.sessions = query(
                conn,
                &top("session_id, session_serial#", "session_id, session_serial#"),
                &params,
                |r| Ok((get(r, 0)?, get(r, 1)?, get(r, 2)?)),
            )?;
        }
    }
    a.blockers = query(
        conn,
        &top(
            "blocking_session, blocking_session_serial#",
            "blocking_session, blocking_session_serial#",
        )
        .replace(" group by", " and blocking_session is not null group by"),
        &params,
        |r| Ok((get(r, 0)?, get(r, 1)?, get(r, 2)?)),
    )?;
    let objects = compat::first_rows(
        &format!(
            "select o.owner || '.' || o.object_name, count(*) \
               from {ash} a join {{objects}} o on o.object_id = a.current_obj# \
              where {} and a.current_obj# > 0 \
              group by o.owner, o.object_name order by count(*) desc",
            filter
                .replace("session_id", "a.session_id")
                .replace("sql_id", "a.sql_id")
                .replace("sample_time", "a.sample_time")
        ),
        10,
    );
    a.objects = with_objects(&objects, |sql| {
        query(conn, sql, &params, |r| Ok((get(r, 0)?, get(r, 1)?)))
    })?;
    Ok(a)
}

/// A statement's executions per AWR snapshot (Diagnostics Pack).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanHistoryRow {
    /// The AWR it is in: a PDB's own, or the database's.
    pub dbid: i64,
    pub snap_id: i64,
    /// When the snapshot ended.
    pub end: String,
    pub plan_hash: i64,
    pub executions: f64,
    pub elapsed: f64,
    pub buffer_gets: f64,
    pub rows: f64,
}

/// When which plan ran, from `DBA_HIST_SQLSTAT`.
pub fn plan_history(conn: &Connection, sql_id: &str) -> Result<Vec<PlanHistoryRow>> {
    query(
        conn,
        "select s.snap_id, to_char(sn.end_interval_time, 'YYYY-MM-DD HH24:MI'), s.plan_hash_value, \
                s.executions_delta, s.elapsed_time_delta / 1e6, s.buffer_gets_delta, s.rows_processed_delta, \
                s.dbid \
           from dba_hist_sqlstat s \
           join dba_hist_snapshot sn \
             on sn.snap_id = s.snap_id and sn.dbid = s.dbid and sn.instance_number = s.instance_number \
          where s.sql_id = :1 \
          order by sn.end_interval_time, s.plan_hash_value",
        &[&sql_id],
        |r| {
            let f = |i| -> Result<f64> { Ok(get::<Option<f64>>(r, i)?.unwrap_or(0.0)) };
            Ok(PlanHistoryRow {
                dbid: get(r, 7)?,
                snap_id: get(r, 0)?,
                end: get::<Option<String>>(r, 1)?.unwrap_or_default(),
                plan_hash: get::<Option<i64>>(r, 2)?.unwrap_or(0),
                executions: f(3)?,
                elapsed: f(4)?,
                buffer_gets: f(5)?,
                rows: f(6)?,
            })
        },
    )
}

/// A plan AWR kept, from the AWR with `dbid` (a plan history row's).
pub fn awr_plan(conn: &Connection, sql_id: &str, plan_hash: i64, dbid: i64) -> Result<Vec<String>> {
    let lines = |r: &oracle::Row| Ok(get::<Option<String>>(r, 0)?.unwrap_or_default());
    if !compat::at_least(conn, 12, 2)? {
        return query(
            conn,
            "select plan_table_output from table(dbms_xplan.display_awr(:1, :2, :3))",
            &[&sql_id, &plan_hash, &dbid],
            lines,
        );
    }
    // A PDB with snapshots of its own keeps them apart from the root's,
    // and its DBA_HIST views show both: look where the row's dbid says,
    // then in the other one, which often keeps the same plan (#93).
    let (con_id, pdb, root): (i64, i64, i64) = conn.query_row_as(
        "select to_number(sys_context('USERENV', 'CON_ID')), con_dbid, dbid from v$database",
        &[],
    )?;
    let lookup = |location: &str, dbid: i64| {
        query(
            conn,
            "select plan_table_output from table(dbms_xplan.display_workload_repository( \
               sql_id => :1, plan_hash_value => :2, dbid => :3, awr_location => :4))",
            &[&sql_id, &plan_hash, &dbid, &location],
            lines,
        )
    };
    if con_id <= 2 {
        return lookup("AWR_ROOT", dbid);
    }
    let (first, other) = if dbid == pdb {
        (("AWR_PDB", pdb), ("AWR_ROOT", root))
    } else {
        (("AWR_ROOT", dbid), ("AWR_PDB", pdb))
    };
    match lookup(first.0, first.1) {
        Ok(plan) if !plan.is_empty() => Ok(plan),
        _ => lookup(other.0, other.1),
    }
}

/// An execution SQL Monitor watched (Tuning Pack): long-running, parallel
/// or hinted `MONITOR`.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitoredExecution {
    pub sql_id: String,
    pub exec_id: i64,
    /// `YYYY-MM-DD HH24:MI:SS`, which with the SQL_ID and the id names it.
    pub start: String,
    pub status: String,
    pub sid: i64,
    pub serial: i64,
    pub user: Option<String>,
    pub elapsed: f64,
    pub cpu: f64,
    pub buffer_gets: f64,
}

/// The executions SQL Monitor has of a session or statement, newest first.
pub fn monitored_executions(conn: &Connection, scope: &Scope) -> Result<Vec<MonitoredExecution>> {
    let (filter, binds) = scope.filter("sid", "session_serial#");
    let params: Vec<&dyn ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    query(
        conn,
        &compat::first_rows(
            &format!(
                "select sql_id, sql_exec_id, to_char(sql_exec_start, 'YYYY-MM-DD HH24:MI:SS'), status, \
                        sid, session_serial#, username, elapsed_time / 1e6, cpu_time / 1e6, buffer_gets \
                   from v$sql_monitor where {filter} and sql_id is not null \
                  order by sql_exec_start desc"
            ),
            20,
        ),
        &params,
        |r| {
            let f = |i| -> Result<f64> { Ok(get::<Option<f64>>(r, i)?.unwrap_or(0.0)) };
            Ok(MonitoredExecution {
                sql_id: get(r, 0)?,
                exec_id: get(r, 1)?,
                start: get::<Option<String>>(r, 2)?.unwrap_or_default(),
                status: get::<Option<String>>(r, 3)?.unwrap_or_default(),
                sid: get(r, 4)?,
                serial: get(r, 5)?,
                user: get(r, 6)?,
                elapsed: f(7)?,
                cpu: f(8)?,
                buffer_gets: f(9)?,
            })
        },
    )
}

/// The SQL Monitor report of one execution, as text or HTML.
pub fn sql_monitor_report(
    conn: &Connection,
    package: &str,
    e: &MonitoredExecution,
    html: bool,
) -> Result<String> {
    let kind = if html { "HTML" } else { "TEXT" };
    let report: Option<String> = conn.query_row_as(
        &format!(
            "select {package}.report_sql_monitor(sql_id => :1, sql_exec_id => :2, \
                    sql_exec_start => to_date(:3, 'YYYY-MM-DD HH24:MI:SS'), type => :4) \
               from dual"
        ),
        &[&e.sql_id, &e.exec_id, &e.start, &kind],
    )?;
    Ok(report.unwrap_or_default())
}

// ----- capabilities ------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capabilities {
    pub user: String,
    /// Probed views: (name, accessible).
    pub views: Vec<(&'static str, bool)>,
    /// `control_management_pack_access`, if readable.
    pub pack_access: Option<String>,
    pub alter_system: bool,
    pub dbms_monitor: bool,
    pub dbms_workload_repository: bool,
    /// The package with REPORT_SQL_MONITOR: DBMS_SQL_MONITOR (12c+), else
    /// DBMS_SQLTUNE.
    pub sql_monitor_package: Option<&'static str>,
    /// The server's release, for statements newer versions have.
    pub release: compat::Release,
}

const PROBED_VIEWS: &[&str] = &[
    "V$SESSION",
    "V$SYSSTAT",
    "V$OSSTAT",
    "V$SESSTAT",
    "V$SQL",
    "V$SQLSTATS",
    "V$LOCK",
    "V$OPEN_CURSOR",
    "V$SESSION_LONGOPS",
    "V$SESSION_WAIT_HISTORY",
    "V$SGA",
    "V$PGASTAT",
    "V$PARAMETER",
    "V$PROCESS",
    "DBA_TABLESPACE_USAGE_METRICS",
    "V$DIAG_TRACE_FILE",
    "V$DIAG_TRACE_FILE_CONTENTS",
];

/// Views of the Diagnostics Pack: not even probed unless it is enabled.
const DIAGNOSTICS_VIEWS: &[&str] = &[
    "DBA_HIST_SNAPSHOT",
    "DBA_HIST_SQLSTAT",
    "V$ACTIVE_SESSION_HISTORY",
];

/// Views of the Tuning Pack, likewise.
const TUNING_VIEWS: &[&str] = &["V$SQL_MONITOR"];

impl Capabilities {
    pub fn can(&self, view: &str) -> bool {
        self.views
            .iter()
            .any(|(v, ok)| *ok && v.eq_ignore_ascii_case(view))
    }

    /// True if the Diagnostics Pack is enabled for this database.
    pub fn diagnostics_licensed(&self) -> bool {
        self.pack_enabled("DIAGNOSTIC")
    }

    pub fn tuning_licensed(&self) -> bool {
        self.pack_enabled("TUNING")
    }

    fn pack_enabled(&self, pack: &str) -> bool {
        self.pack_access
            .as_deref()
            .is_some_and(|v| v.to_ascii_uppercase().contains(pack))
    }

    /// Whether a management pack is enabled, or why `what` isn't offered.
    fn pack(&self, pack: &str, name: &str, what: &str) -> std::result::Result<(), String> {
        match &self.pack_access {
            None => Err(format!(
                "The control_management_pack_access parameter can't be read, so it is unknown whether \
                 the Oracle {name} is licensed. {what} is disabled."
            )),
            Some(_) if !self.pack_enabled(pack) => Err(format!(
                "{what} requires the Oracle {name}, which is not enabled on this database \
                 (control_management_pack_access = {}).",
                self.pack_access.as_deref().unwrap_or("NONE")
            )),
            _ => Ok(()),
        }
    }

    /// Whether Active Session History may be read, or why not.
    pub fn ash(&self) -> std::result::Result<(), String> {
        self.pack("DIAGNOSTIC", "Diagnostics Pack", "Active Session History")?;
        if self.can("V$ACTIVE_SESSION_HISTORY") {
            Ok(())
        } else {
            Err(format!(
                "Active Session History needs SELECT on V$ACTIVE_SESSION_HISTORY ({DICTIONARY_GRANT})."
            ))
        }
    }

    /// Whether a statement's plan history (AWR) may be read, or why not.
    pub fn plan_history(&self) -> std::result::Result<(), String> {
        self.pack("DIAGNOSTIC", "Diagnostics Pack", "Plan history from AWR")?;
        if self.can("DBA_HIST_SQLSTAT") && self.can("DBA_HIST_SNAPSHOT") {
            Ok(())
        } else {
            Err(format!(
                "Plan history needs SELECT on DBA_HIST_SQLSTAT and DBA_HIST_SNAPSHOT ({DICTIONARY_GRANT})."
            ))
        }
    }

    /// Whether SQL Monitor may be used, or why not.
    pub fn sql_monitor(&self) -> std::result::Result<(), String> {
        self.pack("TUNING", "Tuning Pack", "SQL Monitor")?;
        if self.can("V$SQL_MONITOR") && self.sql_monitor_package.is_some() {
            Ok(())
        } else {
            Err(format!(
                "SQL Monitor needs SELECT on V$SQL_MONITOR ({DICTIONARY_GRANT}) and EXECUTE on \
                 DBMS_SQL_MONITOR or DBMS_SQLTUNE."
            ))
        }
    }

    /// Whether AWR/ASH reports may be used, or why not.
    pub fn awr(&self) -> std::result::Result<(), String> {
        self.pack("DIAGNOSTIC", "Diagnostics Pack", "AWR and ASH")?;
        if !self.can("DBA_HIST_SNAPSHOT") || !self.dbms_workload_repository {
            return Err(format!(
                "AWR needs SELECT on DBA_HIST_SNAPSHOT ({DICTIONARY_GRANT}) and EXECUTE on DBMS_WORKLOAD_REPOSITORY."
            ));
        }
        Ok(())
    }

    /// Whether traces can be listed and read through the database.
    pub fn trace_files(&self) -> std::result::Result<(), String> {
        if self.can("V$DIAG_TRACE_FILE") && self.can("V$DIAG_TRACE_FILE_CONTENTS") {
            Ok(())
        } else {
            Err(format!(
                "Reading trace files needs SELECT on V$DIAG_TRACE_FILE and V$DIAG_TRACE_FILE_CONTENTS \
                 (Oracle 12.2+, {DICTIONARY_GRANT}). You can still open a trace file from disk."
            ))
        }
    }

    pub fn tracing(&self) -> std::result::Result<(), String> {
        if self.dbms_monitor {
            Ok(())
        } else {
            Err("Enabling SQL trace needs EXECUTE on DBMS_MONITOR.".into())
        }
    }

    pub fn session_control(&self) -> std::result::Result<(), String> {
        if self.alter_system {
            Ok(())
        } else {
            Err("Killing sessions and cancelling SQL needs the ALTER SYSTEM privilege.".into())
        }
    }

    /// Whether a session's current call can be cancelled, or why not.
    pub fn cancel_sql(&self) -> std::result::Result<(), String> {
        self.session_control()?;
        if self.release >= (18, 0) {
            Ok(())
        } else {
            Err("Cancelling the current SQL of a session needs Oracle 18c or later.".into())
        }
    }

    pub fn missing_views(&self) -> Vec<&'static str> {
        self.views
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(v, _)| *v)
            .collect()
    }
}

pub fn capabilities(conn: &Connection) -> Result<Capabilities> {
    let user: String = conn.query_row_as("select user from dual", &[])?;
    // The packs first: their views aren't touched unless they are enabled.
    let pack_access = conn
        .query_row_as::<Option<String>>(
            "select value from v$parameter where name = 'control_management_pack_access'",
            &[],
        )
        .ok()
        .map(|v| v.unwrap_or_else(|| "NONE".into()));
    let enabled = |pack: &str| {
        pack_access
            .as_deref()
            .is_some_and(|v| v.to_ascii_uppercase().contains(pack))
    };
    let mut probed: Vec<&'static str> = PROBED_VIEWS.to_vec();
    if enabled("DIAGNOSTIC") {
        probed.extend(DIAGNOSTICS_VIEWS);
    }
    if enabled("TUNING") {
        probed.extend(TUNING_VIEWS);
    }
    let views = probed
        .into_iter()
        .map(|v| {
            (
                v,
                conn.query_row_as::<i64>(&format!("select count(*) from {v} where 1 = 0"), &[])
                    .is_ok(),
            )
        })
        .collect();
    let alter_system = conn
        .query_row_as::<i64>(
            "select count(*) from session_privs where privilege = 'ALTER SYSTEM'",
            &[],
        )
        .is_ok_and(|n| n > 0);
    let has_package = |name: &str| {
        conn.query_row_as::<i64>(
            "select count(*) from all_objects where owner = 'SYS' and object_name = :1 and object_type = 'PACKAGE'",
            &[&name],
        )
        .is_ok_and(|n| n > 0)
    };
    Ok(Capabilities {
        user,
        views,
        pack_access,
        alter_system,
        dbms_monitor: has_package("DBMS_MONITOR"),
        dbms_workload_repository: has_package("DBMS_WORKLOAD_REPOSITORY"),
        sql_monitor_package: ["DBMS_SQL_MONITOR", "DBMS_SQLTUNE"]
            .into_iter()
            .find(|p| has_package(p)),
        release: compat::release(conn)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait events between two snapshots: per second, most waited first;
    /// one that appeared counts from zero, one without waits is left out.
    #[test]
    fn wait_event_rates() {
        let e = |event: &str, waits, micros| SystemEvent {
            event: event.into(),
            wait_class: "User I/O".into(),
            waits,
            micros,
        };
        let prev = [
            e("db file sequential read", 100.0, 1e6),
            e("idle-ish", 5.0, 0.0),
        ];
        let cur = [
            e("db file sequential read", 300.0, 3e6),
            e("idle-ish", 5.0, 0.0),
            e("log file sync", 10.0, 5e6),
        ];
        let rates = event_rates(&prev, &cur, 2.0);
        let names: Vec<&str> = rates.iter().map(|r| r.event.as_str()).collect();
        assert_eq!(names, ["log file sync", "db file sequential read"]);
        assert_eq!(rates[0].load, 2.5);
        assert_eq!(rates[0].average_ms, 500.0);
        assert_eq!(rates[1].waits_per_s, 100.0);
        assert_eq!(rates[1].average_ms, 10.0);
    }

    /// A newer read replaces the header and the parts it read, keeps the
    /// others with their errors; once the session is gone, nothing is kept.
    #[test]
    fn detail_merge() {
        let session = SessionRow {
            sid: 1,
            serial: 2,
            ..SessionRow::default()
        };
        let mut d = SessionDetail {
            session: Some(session.clone()),
            overview: Some(SessionOverview::default()),
            sql_text: Some("old".into()),
            stats: vec![("x".into(), 1.0)],
            loaded: vec![DetailTab::Overview, DetailTab::Sql, DetailTab::Stats],
            errors: vec![
                ("Client", "e1".into()),
                ("Statistics", "e2".into()),
                ("SQL", "e3".into()),
                ("Blocking", "e4".into()),
            ],
            ..SessionDetail::default()
        };
        d.merge(SessionDetail {
            session: Some(session),
            sql_text: Some("new".into()),
            loaded: vec![DetailTab::Sql, DetailTab::Columns],
            errors: vec![("Binds", "e5".into())],
            ..SessionDetail::default()
        });
        assert_eq!(d.sql_text.as_deref(), Some("new"));
        assert!(d.overview.is_some());
        assert_eq!(d.stats.len(), 1);
        assert_eq!(
            d.loaded,
            [
                DetailTab::Overview,
                DetailTab::Sql,
                DetailTab::Stats,
                DetailTab::Columns
            ]
        );
        let errors: Vec<&str> = d.errors.iter().map(|(n, _)| *n).collect();
        assert_eq!(errors, ["Client", "Statistics", "Binds"]);
        d.merge(SessionDetail {
            loaded: vec![DetailTab::Sql],
            ..SessionDetail::default()
        });
        assert!(d.session.is_none() && d.overview.is_none() && d.stats.is_empty());
    }

    fn s(sid: i64, blocker: Option<i64>) -> SessionRow {
        SessionRow {
            sid,
            serial: 1,
            status: "ACTIVE".into(),
            blocking_session: blocker,
            ..SessionRow::default()
        }
    }

    fn shape(nodes: &[BlockingNode]) -> Vec<String> {
        fn walk(n: &BlockingNode, depth: usize, out: &mut Vec<String>) {
            out.push(format!("{}{}", "  ".repeat(depth), n.session.sid));
            for c in &n.children {
                walk(c, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        for n in nodes {
            walk(n, 0, &mut out);
        }
        out
    }

    #[test]
    fn chain_and_fanout() {
        // 10 blocks 11 and 12; 12 blocks 13; 20 blocks 21; 30 is unrelated.
        let rows = vec![
            s(10, None),
            s(11, Some(10)),
            s(12, Some(10)),
            s(13, Some(12)),
            s(20, None),
            s(21, Some(20)),
            s(30, None),
        ];
        let tree = build_blocking_tree(&rows);
        assert_eq!(shape(&tree), ["10", "  11", "  12", "    13", "20", "  21"]);
        assert_eq!(tree[0].blocked_count(), 3);
    }

    #[test]
    fn unknown_blocker_gets_placeholder() {
        let tree = build_blocking_tree(&[s(5, Some(99)), s(6, Some(99))]);
        assert_eq!(shape(&tree), ["99", "  5", "  6"]);
        assert_eq!(tree[0].session.status, "UNKNOWN");
    }

    #[test]
    fn cycles_terminate() {
        let tree = build_blocking_tree(&[s(1, Some(2)), s(2, Some(1))]);
        assert_eq!(shape(&tree), ["1", "  2"]);
    }

    #[test]
    fn max_wait() {
        let mut a = s(2, Some(1));
        a.wait_secs = 4.0;
        let mut b = s(3, Some(2));
        b.wait_secs = 9.0;
        let tree = build_blocking_tree(&[s(1, None), a, b]);
        assert_eq!(tree[0].max_wait(), 9.0);
    }

    #[test]
    fn rates() {
        let a = Counters {
            db_time: 1000.0,
            executions: 100.0,
            os_busy: 100.0,
            os_idle: 300.0,
            ..Counters::default()
        };
        let b = Counters {
            db_time: 1400.0,
            executions: 300.0,
            os_busy: 200.0,
            os_idle: 400.0,
            ..Counters::default()
        };
        let r = Rates::between(&a, &b, 2.0);
        assert_eq!(r.aas, 2.0);
        assert_eq!(r.executions, 100.0);
        assert_eq!(r.host_cpu_pct, Some(50.0));
        // Counter reset never yields negative rates.
        assert_eq!(Rates::between(&b, &a, 2.0).executions, 0.0);
    }

    #[test]
    fn licensing_gate() {
        let mut caps = Capabilities {
            views: vec![("DBA_HIST_SNAPSHOT", true)],
            dbms_workload_repository: true,
            ..Capabilities::default()
        };
        assert!(caps.awr().is_err());
        caps.pack_access = Some("NONE".into());
        assert!(caps.awr().unwrap_err().contains("Diagnostics Pack"));
        caps.pack_access = Some("DIAGNOSTIC+TUNING".into());
        assert!(caps.awr().is_ok());
        caps.dbms_workload_repository = false;
        assert!(caps.awr().is_err());
    }

    /// ASH and plan history need the Diagnostics Pack, SQL Monitor the
    /// Tuning Pack, and each its views.
    #[test]
    fn pack_gates() {
        let mut caps = Capabilities {
            views: vec![
                ("V$ACTIVE_SESSION_HISTORY", true),
                ("DBA_HIST_SQLSTAT", true),
                ("DBA_HIST_SNAPSHOT", true),
                ("V$SQL_MONITOR", true),
            ],
            sql_monitor_package: Some("DBMS_SQL_MONITOR"),
            pack_access: Some("DIAGNOSTIC".into()),
            ..Capabilities::default()
        };
        assert!(caps.ash().is_ok() && caps.plan_history().is_ok());
        assert!(caps.sql_monitor().unwrap_err().contains("Tuning Pack"));
        caps.pack_access = Some("DIAGNOSTIC+TUNING".into());
        assert!(caps.sql_monitor().is_ok());
        caps.pack_access = Some("NONE".into());
        assert!(caps.ash().unwrap_err().contains("Diagnostics Pack"));
        caps.pack_access = None;
        assert!(caps.plan_history().unwrap_err().contains("unknown"));
    }
}
