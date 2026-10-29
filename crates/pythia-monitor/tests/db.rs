//! Integration tests against a real database.
//!
//! Skipped unless `PYTHIA_TEST_DB_DBA` is set (a DBA account), e.g.
//! `PYTHIA_TEST_DB_DBA=system/oracle@localhost:1521/FREEPDB1`. The app-user
//! capability test also uses `PYTHIA_TEST_DB` (e.g. `pythia/pythia@...`) if set.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use pythia_db::oracle::Connection;
use pythia_monitor::queries::{self, DetailTab, SessionFilter};
use pythia_monitor::tkprof::{self, CallKind};

fn connect(var: &str) -> Option<Connection> {
    let spec = std::env::var(var).ok()?;
    let (cred, db) = spec.rsplit_once('@')?;
    let (user, pass) = cred.split_once('/')?;
    Some(Connection::connect(user, pass, db).expect("connect"))
}

fn dba() -> Option<Connection> {
    let c = connect("PYTHIA_TEST_DB_DBA");
    if c.is_none() {
        eprintln!("PYTHIA_TEST_DB_DBA not set, skipping");
    }
    c
}

fn sid(c: &Connection) -> i64 {
    c.query_row_as(
        "select to_number(sys_context('USERENV', 'SID')) from dual",
        &[],
    )
    .unwrap()
}

/// Polls until `f` returns true or the timeout passes.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// Accounts with some `V$` grants but no SELECT_CATALOG_ROLE (#90): with
/// `V$SESSION` but not `V$SQL` (GSMADMIN_ROLE) the sessions show without
/// their SQL text; without `V$OSSTAT` (DV_SECANALYST) the instance shows
/// without its CPU count.
#[test]
fn sessions_and_instance_with_few_grants() {
    let Some(dba) = dba() else { return };
    let spec = std::env::var("PYTHIA_TEST_DB_DBA").unwrap();
    let db = spec.rsplit_once('@').unwrap().1;
    let cleanup = "begin
           for u in (select username from dba_users
                      where username in ('PYTHIA_MON_SESSIONS', 'PYTHIA_MON_INSTANCE')) loop
             execute immediate 'drop user ' || u.username || ' cascade';
           end loop;
         end;";
    dba.execute(cleanup, &[]).unwrap();
    // Some releases lack one of the roles: that part is skipped.
    let account = |name: &str, role: &str| -> Option<Connection> {
        dba.execute(
            &format!("create user {name} identified by \"Pythia_mon_1\""),
            &[],
        )
        .unwrap();
        dba.execute(&format!("grant create session to {name}"), &[])
            .unwrap();
        if let Err(e) = dba.execute(&format!("grant {role} to {name}"), &[]) {
            eprintln!("{role}: {e}, skipping");
            return None;
        }
        Some(Connection::connect(name, "Pythia_mon_1", db).unwrap())
    };
    let readable = |c: &Connection, view: &str| {
        c.query_row_as::<i64>(&format!("select count(*) from {view} where 1 = 0"), &[])
            .is_ok()
    };

    if let Some(c) = account("pythia_mon_sessions", "gsmadmin_role") {
        assert!(readable(&c, "v$session") && !readable(&c, "v$sql"));
        let me = sid(&c);
        let rows = queries::sessions(&c, SessionFilter::default()).unwrap();
        let mine = rows
            .iter()
            .find(|r| r.sid == me)
            .expect("own session listed");
        assert!(mine.sql_text.is_none());
        queries::blocking_tree(&c).unwrap();
        let detail = queries::session_details(&c, mine.sid, mine.serial, &[DetailTab::Overview]);
        assert!(detail.session.is_some(), "{:?}", detail.errors);
        drop(c);
    }
    if let Some(c) = account("pythia_mon_instance", "dv_secanalyst") {
        assert!(!readable(&c, "v$osstat"));
        let inst = queries::instance_info(&c).unwrap();
        assert!(inst.cpus.is_none() && !inst.instance.is_empty());
        let snap = queries::dashboard_snapshot(&c);
        assert!(snap.instance.is_some());
        assert!(
            !snap.errors.iter().any(|(name, _)| *name == "Instance"),
            "{:?}",
            snap.errors
        );
        drop(c);
    }
    dba.execute(cleanup, &[]).unwrap();
}

#[test]
fn capabilities_dba_and_app_user() {
    let Some(conn) = dba() else { return };
    let caps = queries::capabilities(&conn).unwrap();
    // Trace files can be read through the database from 12.2 on.
    let traces = caps.release >= (12, 2);
    let missing: Vec<_> = caps
        .missing_views()
        .into_iter()
        .filter(|v| traces || !v.starts_with("V$DIAG_TRACE_FILE"))
        .collect();
    assert!(missing.is_empty(), "missing: {missing:?}");
    assert!(caps.alter_system && caps.dbms_monitor);
    assert!(caps.session_control().is_ok());
    assert_eq!(caps.cancel_sql().is_ok(), caps.release >= (18, 0));
    assert_eq!(caps.trace_files().is_ok(), traces);
    // Oracle Free enables the packs; the gate must allow AWR there.
    assert!(caps.pack_access.is_some());
    assert_eq!(caps.awr().is_ok(), caps.diagnostics_licensed());

    // The unprivileged checks only hold for a user without catalog access.
    let catalog_role = |c: &Connection| {
        c.query_row_as::<i64>(
            "select count(*) from session_roles where role = 'SELECT_CATALOG_ROLE'",
            &[],
        )
        .unwrap()
            > 0
    };
    if let Some(app) = connect("PYTHIA_TEST_DB").filter(|c| !catalog_role(c)) {
        let caps = queries::capabilities(&app).unwrap();
        assert!(!caps.missing_views().is_empty());
        assert!(caps.awr().is_err());
        assert!(caps.session_control().is_err());
        // The dashboard degrades per section instead of failing.
        let snap = queries::dashboard_snapshot(&app);
        assert!(!snap.errors.is_empty());
        assert!(
            snap.errors
                .iter()
                .any(|(_, e)| e.contains("SELECT_CATALOG_ROLE")),
            "{:?}",
            snap.errors
        );
    }
}

#[test]
fn dashboard_snapshot() {
    let Some(conn) = dba() else { return };
    let a = queries::dashboard_snapshot(&conn);
    assert!(a.errors.is_empty(), "{:?}", a.errors);
    let inst = a.instance.as_ref().unwrap();
    assert!(inst.cpus.is_some_and(|n| n >= 1) && inst.uptime_secs > 0);
    assert!(a.sessions.unwrap().total > 0);
    assert!(a.open_cursors.unwrap() > 0);
    assert!(!a.tablespaces.is_empty());
    assert!(!a.top_sql.is_empty());
    assert!(a.memory.as_ref().is_some_and(|m| !m.sga.is_empty()));

    for _ in 0..50 {
        conn.query_row_as::<i64>("select count(*) from all_objects where rownum < 100", &[])
            .unwrap();
    }
    let b = queries::dashboard_snapshot(&conn);
    let rates = queries::Rates::between(&a.counters.unwrap(), &b.counters.unwrap(), 1.0);
    assert!(rates.executions >= 50.0, "{rates:?}");
    assert!(rates.logical_reads > 0.0);
}

#[test]
fn blocking_chain_cancel_and_kill() {
    let Some(conn) = dba() else { return };
    let _ = conn.execute("drop table pythia_mon_lock purge", &[]);
    conn.execute(
        "create table pythia_mon_lock (id number primary key, v number)",
        &[],
    )
    .unwrap();
    conn.execute("insert into pythia_mon_lock values (1, 0)", &[])
        .unwrap();
    conn.commit().unwrap();

    let spec = std::env::var("PYTHIA_TEST_DB_DBA").unwrap();
    let open = || {
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (u, p) = cred.split_once('/').unwrap();
        Connection::connect(u, p, db).unwrap()
    };

    // A holds the row lock.
    let a = open();
    let a_sid = sid(&a);
    a.execute("update pythia_mon_lock set v = 1 where id = 1", &[])
        .unwrap();

    // B and C wait for it in their own threads.
    let spawn_waiter = |tag: &'static str| {
        let c = open();
        let my_sid = sid(&c);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            let r = c.execute(
                &format!("update pythia_mon_lock set v = 2 where id = 1 /* {tag} */"),
                &[],
            );
            let outcome = r
                .map(|_| ())
                .map_err(|e| e.db_error().map(|d| d.code()).unwrap_or(-1));
            // Hold any lock we got until told to finish.
            let _ = done_rx.recv();
            let _ = c.rollback();
            outcome
        });
        (my_sid, done_tx, handle)
    };
    let (b_sid, b_done, b_thread) = spawn_waiter("B");
    let (c_sid, c_done, c_thread) = spawn_waiter("C");

    wait_for("two blocked sessions", || {
        queries::sessions(&conn, SessionFilter::default())
            .unwrap()
            .iter()
            .filter(|s| s.blocking_session == Some(a_sid))
            .count()
            == 2
    });

    // The dashboard names the final blocker and the insight points at it.
    let snap = queries::dashboard_snapshot(&conn);
    let blocker = snap
        .blockers
        .iter()
        .find(|b| b.sid == a_sid)
        .unwrap_or_else(|| panic!("blockers {:?} errors {:?}", snap.blockers, snap.errors));
    assert_eq!(blocker.blocked, 2);
    assert_eq!(blocker.status, "INACTIVE");
    let found = pythia_monitor::insights::insights(&snap, None);
    assert!(
        found
            .iter()
            .any(|i| i.text.contains(&format!("Session {a_sid} "))),
        "{found:?}"
    );

    let tree = queries::blocking_tree(&conn).unwrap();
    let root = tree
        .iter()
        .find(|n| n.session.sid == a_sid)
        .expect("A is a root blocker");
    let mut kids: Vec<i64> = root.children.iter().map(|c| c.session.sid).collect();
    kids.sort_unstable();
    let mut expected = vec![b_sid, c_sid];
    expected.sort_unstable();
    assert_eq!(kids, expected);
    let waiter = &root.children[0].session;
    assert!(
        waiter.event.as_deref().unwrap_or("").starts_with("enq: TX"),
        "{waiter:?}"
    );
    assert!(
        waiter
            .wait_object
            .as_deref()
            .unwrap_or("")
            .ends_with(".PYTHIA_MON_LOCK"),
        "{waiter:?}"
    );
    assert!(waiter.wait_rowid.is_some());
    assert!(
        waiter
            .sql_text
            .as_deref()
            .unwrap_or("")
            .contains("pythia_mon_lock")
    );

    // Details of a waiter: the lock request is visible.
    let detail = queries::session_detail(&conn, waiter.sid, waiter.serial);
    assert!(detail.errors.is_empty(), "{:?}", detail.errors);
    assert!(
        detail
            .locks
            .iter()
            .any(|l| l.lock_type == "TX" && l.mode_requested != "None"),
        "{:?}",
        detail.locks
    );
    assert!(
        detail
            .sql_text
            .as_deref()
            .unwrap_or("")
            .contains("pythia_mon_lock")
    );
    assert!(!detail.plan.is_empty());
    assert!(!detail.stats.is_empty());
    // And the blocker holds the TM lock on the table.
    let a_locks = queries::locks(&conn, a_sid).unwrap();
    assert!(a_locks.iter().any(|l| l.blocking), "{a_locks:?}");
    // The waiter knows its blocker's serial# (a link to its details); the
    // blocker's details list both waiters and its transaction, which a
    // kill would roll back.
    let a_serial = root.session.serial;
    assert_eq!(waiter.blocking_serial, Some(a_serial));
    let a_detail = queries::session_detail(&conn, a_sid, a_serial);
    let mut blocked: Vec<i64> = a_detail.blocks.iter().map(|b| b.0).collect();
    blocked.sort_unstable();
    assert_eq!(blocked, expected);
    let tx = a_detail.transaction.expect("A's transaction");
    assert!(tx.undo_records >= 1, "{tx:?}");
    assert_eq!(
        queries::transaction(&conn, a_sid, a_serial).unwrap(),
        Some(tx)
    );
    assert_eq!(
        queries::transaction(&conn, b_sid, waiter_serial(&conn, b_sid)).unwrap(),
        None
    );

    // Cancel C's statement: its update fails with ORA-01013.
    let c_serial = queries::sessions(&conn, SessionFilter::default())
        .unwrap()
        .into_iter()
        .find(|s| s.sid == c_sid)
        .unwrap()
        .serial;
    if pythia_db::compat::at_least(&conn, 18, 0).unwrap() {
        queries::cancel_sql(&conn, c_sid, c_serial).unwrap();
        c_done.send(()).unwrap();
        assert_eq!(c_thread.join().unwrap(), Err(1013));
    } else {
        // Before 18c a call can't be cancelled from outside, only killed.
        assert!(queries::cancel_sql(&conn, c_sid, c_serial).is_err());
        queries::kill_session(&conn, c_sid, c_serial, true).unwrap();
        c_done.send(()).unwrap();
        assert!(c_thread.join().unwrap().is_err());
    }

    // Releasing A lets B through; then B is no longer blocked.
    a.rollback().unwrap();
    wait_for("B to get the lock", || {
        queries::sessions(&conn, SessionFilter::default())
            .unwrap()
            .iter()
            .all(|s| s.blocking_session.is_none() || s.sid != b_sid)
    });
    b_done.send(()).unwrap();
    assert_eq!(b_thread.join().unwrap(), Ok(()));

    // Kill an idle session.
    let d = open();
    let d_sid = sid(&d);
    let d_serial = queries::sessions(&conn, SessionFilter::default())
        .unwrap()
        .into_iter()
        .find(|s| s.sid == d_sid)
        .unwrap()
        .serial;
    queries::kill_session(&conn, d_sid, d_serial, true).unwrap();
    assert!(d.query_row_as::<i64>("select 1 from dual", &[]).is_err());

    drop(a);
    conn.execute("drop table pythia_mon_lock purge", &[])
        .unwrap();
}

fn waiter_serial(conn: &Connection, sid: i64) -> i64 {
    queries::sessions(conn, SessionFilter::default())
        .unwrap()
        .into_iter()
        .find(|s| s.sid == sid)
        .unwrap()
        .serial
}

/// Disconnect after transaction: the session stays until its transaction
/// ends, then goes.
#[test]
fn disconnect_after_transaction() {
    let Some(conn) = dba() else { return };
    let e = open_dba();
    let (e_sid, e_serial) = (sid(&e), serial(&e));
    e.execute(
        "create global temporary table pythia_mon_disc (n number) on commit delete rows",
        &[],
    )
    .ok();
    e.execute("insert into pythia_mon_disc values (1)", &[])
        .unwrap();
    queries::disconnect_session(&conn, e_sid, e_serial).unwrap();
    // Still there with its transaction.
    let n: i64 = e
        .query_row_as("select count(*) from pythia_mon_disc", &[])
        .unwrap();
    assert_eq!(n, 1);
    // Ending it ends the session (ORA-00028 on its next call).
    e.rollback().unwrap();
    assert!(e.query_row_as::<i64>("select 1 from dual", &[]).is_err());
    wait_for("the session to go", || {
        !queries::sessions(&conn, SessionFilter::default())
            .unwrap()
            .iter()
            .any(|s| s.sid == e_sid && s.serial == e_serial)
    });
}

#[test]
fn trace_round_trip() {
    let Some(conn) = dba() else { return };
    let traced = {
        let spec = std::env::var("PYTHIA_TEST_DB_DBA").unwrap();
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (u, p) = cred.split_once('/').unwrap();
        Connection::connect(u, p, db).unwrap()
    };
    if !pythia_db::compat::at_least(&conn, 12, 2).unwrap() {
        eprintln!("trace files need 12.2, skipping");
        return;
    }
    let traced_sid = sid(&traced);
    queries::trace_self(&traced, true).unwrap();
    for i in 0..3 {
        let n: i64 = traced
            .query_row_as(
                "select /* orui_trace_marker */ count(*) from all_objects where object_id < :1",
                &[&(1000 + i)],
            )
            .unwrap();
        assert!(n > 0);
    }
    traced
        .execute("begin null; /* orui_trace_plsql */ end;", &[])
        .unwrap();
    let traced_now = |conn: &Connection| {
        queries::sessions(conn, SessionFilter::default())
            .unwrap()
            .into_iter()
            .find(|s| s.sid == traced_sid)
            .unwrap()
            .sql_trace
    };
    assert!(traced_now(&conn), "the session list says it's traced");
    queries::trace_self(&traced, false).unwrap();
    assert!(!traced_now(&conn));

    let (dir, file) = queries::session_trace_file(&conn, traced_sid)
        .unwrap()
        .expect("trace file");
    // The file shows up in the list and can be read through the database.
    let files = queries::trace_files(&conn, &file, 5).unwrap();
    assert!(files.iter().any(|f| f.name == file), "{files:?}");
    let text = queries::read_trace_file(&conn, &dir, &file).unwrap();
    assert!(text.contains("PARSING IN CURSOR"));

    if let Ok(path) = std::env::var("PYTHIA_WRITE_TRACE_FIXTURE") {
        std::fs::write(path, &text).unwrap();
    }

    let report = tkprof::parse(&text);
    let marker = report
        .statements
        .iter()
        .find(|s| s.text.contains("orui_trace_marker"))
        .expect("marker statement");
    assert_eq!(marker.call(CallKind::Execute).count, 3);
    assert_eq!(marker.call(CallKind::Fetch).rows, 3);
    assert!(marker.total().query > 0);
    assert_eq!(marker.binds.first().map(String::as_str), Some("1000"));
    assert!(!marker.row_source.is_empty());
    assert!(
        report
            .statements
            .iter()
            .any(|s| s.text.contains("orui_trace_plsql"))
    );
    assert!(
        report
            .to_text(tkprof::SortKey::TotalElapsed)
            .contains("orui_trace_marker")
    );
}

#[test]
fn awr_and_ash() {
    let Some(conn) = dba() else { return };
    let caps = queries::capabilities(&conn).unwrap();
    if caps.awr().is_err() {
        eprintln!("AWR not available: {:?}", caps.awr());
        return;
    }
    let before = queries::awr_snapshots(&conn, 10).unwrap();
    // Two of our own: a report over an older snapshot fails with ORA-20019
    // when the instance was restarted since.
    queries::awr_create_snapshot(&conn).unwrap();
    let id = queries::awr_create_snapshot(&conn).unwrap();
    let snaps = queries::awr_snapshots(&conn, 10).unwrap();
    assert!(
        snaps.len() > before.len() || snaps.first().map(|s| s.snap_id) == Some(id),
        "{snaps:?}"
    );
    if snaps.len() >= 2 {
        let (end, begin) = (&snaps[0], &snaps[1]);
        let text = queries::awr_report(
            &conn,
            end.dbid,
            end.instance,
            begin.snap_id,
            end.snap_id,
            false,
        )
        .unwrap();
        assert!(
            text.to_uppercase().contains("WORKLOAD REPOSITORY"),
            "{}",
            &text[..text.len().min(500)]
        );
        let html = queries::awr_report(
            &conn,
            end.dbid,
            end.instance,
            begin.snap_id,
            end.snap_id,
            true,
        )
        .unwrap();
        assert!(html.to_lowercase().contains("<html"));
    }
    let ash = queries::ash_report(&conn, 30, false).unwrap();
    assert!(
        ash.to_uppercase().contains("ASH REPORT"),
        "{}",
        &ash[..ash.len().min(500)]
    );
}

fn open_dba() -> Connection {
    connect("PYTHIA_TEST_DB_DBA").unwrap()
}

fn serial(c: &Connection) -> i64 {
    c.query_row_as(
        "select serial# from v$session where sid = sys_context('USERENV', 'SID')",
        &[],
    )
    .unwrap()
}

/// Details of a session that has ended don't show the session that got its
/// SID since: the views without serial# aren't read (#75).
#[test]
fn session_detail_of_a_reused_sid() {
    let Some(conn) = dba() else { return };
    let other = open_dba();
    let (other_sid, other_serial) = (sid(&other), serial(&other));
    let detail = queries::session_detail(&conn, other_sid, other_serial);
    assert!(detail.session.is_some());
    assert!(!detail.stats.is_empty());
    // Another serial#: the session the details were for is gone.
    let detail = queries::session_detail(&conn, other_sid, other_serial + 1);
    assert!(detail.session.is_none());
    assert!(detail.stats.is_empty(), "{:?}", detail.stats);
    assert!(detail.waits.is_empty() && detail.cursors.is_empty() && detail.locks.is_empty());
    assert!(detail.errors.is_empty(), "{:?}", detail.errors);
}

/// Open cursors show whole statements, not V$OPEN_CURSOR's first 60
/// characters: up to 1000 from V$SQL.SQL_TEXT, longer ones in full (#73).
#[test]
fn open_cursors_show_whole_statements() {
    let Some(conn) = dba() else { return };
    let medium = "select /* pythia_cursor_medium: a statement longer than sixty characters */ \
                  dummy from dual"
        .to_owned();
    let long = format!(
        "select /* pythia_cursor_long */ 1{} as total from dual",
        " + 1".repeat(400)
    );
    assert!(long.len() > 1000);
    let other = open_dba();
    // DBMS_SQL cursors stay open until closed (these never are).
    for statement in [&medium, &long] {
        other
            .execute(
                "declare c integer; begin c := dbms_sql.open_cursor; \
                 dbms_sql.parse(c, :1, dbms_sql.native); end;",
                &[statement],
            )
            .unwrap();
    }
    let detail = queries::session_detail(&conn, sid(&other), serial(&other));
    assert!(detail.errors.is_empty(), "{:?}", detail.errors);
    for statement in [medium, long] {
        assert!(
            detail.cursors.iter().any(|c| c.text == statement),
            "{statement}: {:?}",
            detail.cursors
        );
    }
}

/// Object names of locks and row lock waits on tables the monitoring user
/// has no privileges on: ALL_OBJECTS doesn't list them, DBA_OBJECTS does
/// (#76). Needs `PYTHIA_TEST_DB`, a user with SELECT_CATALOG_ROLE only.
#[test]
fn lock_names_of_other_schemas() {
    let (Some(dba), Some(monitor)) = (dba(), connect("PYTHIA_TEST_DB")) else {
        return;
    };
    let _ = dba.execute("drop table pythia_mon_names purge", &[]);
    dba.execute("create table pythia_mon_names (id number primary key)", &[])
        .unwrap();
    dba.execute("insert into pythia_mon_names values (1)", &[])
        .unwrap();
    dba.commit().unwrap();
    let owner: String = dba.query_row_as("select user from dual", &[]).unwrap();
    let name = format!("{owner}.PYTHIA_MON_NAMES");
    let hidden: i64 = monitor
        .query_row_as(
            "select count(*) from all_objects where owner = :1 and object_name = 'PYTHIA_MON_NAMES'",
            &[&owner],
        )
        .unwrap();
    assert_eq!(hidden, 0, "the monitoring user can't see the table");

    // A holds a row lock (and the table's TM lock), B waits for the row.
    let a = open_dba();
    let a_sid = sid(&a);
    a.execute("update pythia_mon_names set id = 1 where id = 1", &[])
        .unwrap();
    let b = open_dba();
    let b_sid = sid(&b);
    let waiter = thread::spawn(move || {
        let _ = b.execute("update pythia_mon_names set id = 1 where id = 1", &[]);
        let _ = b.rollback();
    });
    wait_for("the waiter", || {
        queries::sessions(&monitor, SessionFilter::default())
            .unwrap()
            .iter()
            .any(|s| s.sid == b_sid && s.blocking_session == Some(a_sid))
    });
    let b_row = queries::sessions(&monitor, SessionFilter::default())
        .unwrap()
        .into_iter()
        .find(|s| s.sid == b_sid)
        .unwrap();
    assert_eq!(b_row.wait_object.as_deref(), Some(name.as_str()));
    assert!(b_row.wait_rowid.is_some());
    let locks = queries::locks(&monitor, a_sid).unwrap();
    assert!(
        locks
            .iter()
            .any(|l| l.lock_type == "TM" && l.object.as_deref() == Some(name.as_str())),
        "{locks:?}"
    );
    a.rollback().unwrap();
    waiter.join().unwrap();
    dba.execute("drop table pythia_mon_names purge", &[])
        .unwrap();
}

/// Session details read the parts asked for (#74): the previous SQL with
/// its captured binds and plan, the V$SESSION row, and the overview with
/// the PL/SQL a session runs.
#[test]
fn session_detail_parts() {
    let Some(conn) = dba() else { return };
    let other = open_dba();
    let (other_sid, other_serial) = (sid(&other), serial(&other));
    // A statement of its own (hard parsed), so its bind is captured.
    let marker = format!(
        "pythia_binds_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let n: i64 = other
        .query_row_as(
            &format!("select /* {marker} */ count(*) from dual where dummy = :1"),
            &[&"X"],
        )
        .unwrap();
    assert_eq!(n, 1);
    let parts = [DetailTab::Sql, DetailTab::Plan, DetailTab::Columns];
    let d = queries::session_details(&conn, other_sid, other_serial, &parts);
    assert!(d.errors.is_empty(), "{:?}", d.errors);
    assert_eq!(d.loaded, parts);
    assert!(
        d.prev_sql_text.as_deref().unwrap_or("").contains(&marker),
        "{:?}",
        d.prev_sql_text
    );
    assert!(
        d.prev_binds.iter().any(|b| b.value.as_deref() == Some("X")),
        "{:?}",
        d.prev_binds
    );
    assert!(!d.plan.is_empty());
    assert!(
        d.columns
            .iter()
            .any(|(c, v)| c == "SID" && *v == other_sid.to_string()),
        "{:?}",
        d.columns
    );
    assert!(
        d.overview.is_none() && d.stats.is_empty(),
        "only the parts asked for"
    );

    // While it runs a package's procedure, the overview names it.
    conn.execute(
        "create or replace package pythia_mon_plsql as procedure wait_a_bit; end;",
        &[],
    )
    .unwrap();
    conn.execute(
        "create or replace package body pythia_mon_plsql as \
           procedure wait_a_bit is begin dbms_session.sleep(3); end; end;",
        &[],
    )
    .unwrap();
    let running = thread::spawn(move || {
        other
            .execute("begin pythia_mon_plsql.wait_a_bit; end;", &[])
            .unwrap();
    });
    let mut overview = None;
    wait_for("the PL/SQL it runs", || {
        let d = queries::session_details(&conn, other_sid, other_serial, &[DetailTab::Overview]);
        let o = d.overview.unwrap();
        let found = o
            .plsql
            .iter()
            .any(|u| u.name == "PYTHIA_MON_PLSQL" && u.subprogram.as_deref() == Some("WAIT_A_BIT"));
        if found {
            overview = Some(o);
        }
        found
    });
    let o = overview.unwrap();
    eprintln!("{o:#?}");
    assert!(
        o.client.iter().any(|(n, _)| n == "Program"),
        "{:?}",
        o.client
    );
    assert!(o.process.is_some());
    assert!(!o.time_model.is_empty());
    assert!(o.connection.iter().any(|(n, _)| n == "Network encryption"));
    running.join().unwrap();
    conn.execute("drop package pythia_mon_plsql", &[]).unwrap();
}

/// SQL details (#74): text, totals, child cursors (a second one for a bind
/// of another type, with the reason), the plan and captured binds of a
/// child.
#[test]
fn sql_details_of_a_statement() {
    let Some(conn) = dba() else { return };
    let other = open_dba();
    let marker = format!(
        "pythia_sqldetail_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let sql = format!("select /* {marker} */ count(*) from dual where :1 is not null");
    let n: i64 = other.query_row_as(&sql, &[&"X"]).unwrap();
    assert_eq!(n, 1);
    // A number this time: the cursor can't be shared (bind mismatch).
    let n: i64 = other.query_row_as(&sql, &[&42_i64]).unwrap();
    assert_eq!(n, 1);
    let sql_id: String = conn
        .query_row_as(
            "select sql_id from v$sql where sql_text like '%' || :1 || '%' \
               and sql_text not like '%v$sql%' and rownum = 1",
            &[&marker],
        )
        .unwrap();

    let d = queries::sql_detail(&conn, &sql_id, None);
    assert!(d.errors.is_empty(), "{:?}", d.errors);
    assert!(d.text.as_deref().unwrap_or("").contains(&marker));
    assert!(d.stats.as_ref().unwrap().executions >= 2.0, "{:?}", d.stats);
    assert!(d.children.len() >= 2, "{:?}", d.children);
    assert!(!d.plan.is_empty());
    assert!(!d.plan_rows.is_empty(), "the plan's lines too");
    assert!(d.child.is_some());

    let first = d.children[0].child;
    let d = queries::sql_detail(&conn, &sql_id, Some(first));
    assert_eq!(d.child, Some(first));
    assert!(
        d.binds.iter().any(|b| b.value.as_deref() == Some("X")),
        "{:?}",
        d.binds
    );
    let second = d.children[1].child;
    let d = queries::sql_detail(&conn, &sql_id, Some(second));
    eprintln!("not shared: {:?}", d.not_shared);
    assert!(!d.not_shared.is_empty(), "why child {second} isn't shared");

    // A statement gone from the shared pool.
    let d = queries::sql_detail(&conn, "0000000000000", None);
    assert!(d.text.is_none() && d.children.is_empty() && d.errors.is_empty());
}

/// A statement that collected row source statistics: its plan lines have
/// the last execution's actual rows next to the estimates, in the SQL
/// details and in the details of the session that ran it (#152).
#[test]
fn plans_with_actual_rows() {
    let Some(conn) = dba() else { return };
    let other = open_dba();
    // Before the statement: it is the session's previous one then.
    let (other_sid, other_serial) = (sid(&other), serial(&other));
    let marker = format!(
        "pythia_actuals_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // Estimated at 1% of the rows, all of them match.
    let sql = format!(
        "select /*+ gather_plan_statistics */ /* {marker} */ count(*) \
           from (select level n from dual connect by level <= 3000) \
          where upper(to_char(n)) = to_char(n)"
    );
    let n: i64 = other.query_row_as(&sql, &[]).unwrap();
    assert_eq!(n, 3000);
    let sql_id: String = conn
        .query_row_as(
            "select sql_id from v$sql where sql_text like '%' || :1 || '%' \
               and sql_text not like '%v$sql%' and rownum = 1",
            &[&marker],
        )
        .unwrap();

    let d = queries::sql_detail(&conn, &sql_id, None);
    assert!(d.errors.is_empty(), "{:?}", d.errors);
    assert!(
        pythia_ui::plan::has_actuals(&d.plan_rows),
        "{:?}",
        d.plan_rows
    );
    assert!(d.plan.iter().any(|l| l.contains("A-Rows")), "{:?}", d.plan);
    let count = d
        .plan_rows
        .iter()
        .find(|r| r.operation.starts_with("SORT AGGREGATE"))
        .expect("the count");
    assert_eq!(count.actual_rows, Some(1.0));
    let (_, note) = pythia_ui::plan::misestimate_note(&d.plan_rows).expect("a misestimate");
    assert!(note.contains("got 3,000"), "{note}");

    let parts = [DetailTab::Plan];
    let s = queries::session_details(&conn, other_sid, other_serial, &parts);
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert!(
        pythia_ui::plan::has_actuals(&s.plan_rows),
        "{:?}",
        s.plan_rows
    );
    assert!(s.plan.iter().any(|l| l.contains("A-Rows")), "{:?}", s.plan);

    // Without statistics: estimates only.
    let d = queries::sql_detail(&conn, "0000000000000", None);
    assert!(d.plan_rows.is_empty());
}

/// Wait events for the dashboard's rates and an event's histogram (#74).
#[test]
fn wait_events_and_histogram() {
    let Some(conn) = dba() else { return };
    let events = queries::system_events(&conn).unwrap();
    assert!(!events.is_empty());
    assert!(events.iter().all(|e| e.wait_class != "Idle"));
    let busiest = events
        .iter()
        .max_by(|a, b| a.waits.total_cmp(&b.waits))
        .unwrap();
    let buckets = queries::event_histogram(&conn, &busiest.event).unwrap();
    assert!(!buckets.is_empty(), "{busiest:?}");
    assert!(buckets.windows(2).all(|w| w[0].0 < w[1].0), "{buckets:?}");
    let snap = queries::dashboard_snapshot(&conn);
    assert!(!snap.events.is_empty(), "{:?}", snap.errors);
}

/// What the management packs keep about a session and a statement (#74):
/// activity from ASH, a filtered ASH report, plan history from AWR and SQL
/// Monitor's executions and report. Skips a pack that isn't enabled.
#[test]
fn management_pack_history() {
    use queries::Scope;
    let Some(conn) = dba() else { return };
    let caps = queries::capabilities(&conn).unwrap();
    let worker = open_dba();
    let (worker_sid, worker_serial) = (sid(&worker), serial(&worker));
    let marker = format!(
        "pythia_history_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // On CPU for about three seconds: ASH samples it.
    worker
        .execute(
            "declare n number := 0; t pls_integer := dbms_utility.get_time; \
             begin while dbms_utility.get_time - t < 300 loop n := n + 1; end loop; end;",
            &[],
        )
        .unwrap();
    // Hinted MONITOR: SQL Monitor watches it however short.
    let sql = format!(
        "select /*+ monitor */ /* {marker} */ count(*) from dual connect by level <= 10000"
    );
    let n: i64 = worker.query_row_as(&sql, &[]).unwrap();
    assert_eq!(n, 10000);
    let sql_id: String = conn
        .query_row_as(
            "select sql_id from v$sql where sql_text like '%' || :1 || '%' \
               and sql_text not like '%v$sql%' and rownum = 1",
            &[&marker],
        )
        .unwrap();
    let session = Scope::Session {
        sid: worker_sid,
        serial: worker_serial,
    };

    if caps.ash().is_ok() {
        let mut activity = None;
        wait_for("ASH samples of the session", || {
            let a = queries::ash_activity(&conn, &session, 10).unwrap();
            let found = a.samples > 0;
            activity = Some(a);
            found
        });
        let a = activity.unwrap();
        assert!(a.timeline.iter().any(|t| t.1 == "CPU"), "{a:?}");
        assert!(a.events.iter().any(|e| e.0 == "ON CPU"), "{a:?}");
        let report = queries::ash_report_for(&conn, Some(&session), 10, false).unwrap();
        assert!(!report.trim().is_empty());
    } else {
        eprintln!("Diagnostics Pack not enabled, skipping ASH");
    }

    if caps.plan_history().is_ok() {
        // Colored, AWR keeps it whatever it cost: run again after coloring,
        // so the next snapshot (also another test's) has it.
        conn.execute(
            "begin dbms_workload_repository.add_colored_sql(:1); end;",
            &[&sql_id],
        )
        .unwrap();
        // Under load (the whole suite) the cursor may age out of the small
        // shared pool before the snapshot, and a snapshot on a slow machine
        // (CI) may keep the statement's statistics without its plan (#93):
        // run and snapshot again until AWR has both.
        let has_plan = |plan: &[String]| plan.iter().any(|l| l.contains("CONNECT BY"));
        let mut history = Vec::new();
        let mut plan = Vec::new();
        for _ in 0..5 {
            let _: i64 = worker.query_row_as(&sql, &[]).unwrap();
            queries::awr_create_snapshot(&conn).unwrap();
            history = queries::plan_history(&conn, &sql_id).unwrap();
            if let Some(newest) = history.last() {
                plan = queries::awr_plan(&conn, &sql_id, newest.plan_hash, newest.dbid).unwrap();
                if has_plan(&plan) {
                    break;
                }
            }
        }
        assert!(!history.is_empty(), "the colored statement is in AWR");
        // By time, though the root's and the PDB's snapshot ids differ
        // (#93): the newest is this test's own snapshot.
        assert!(
            history.windows(2).all(|w| w[0].end <= w[1].end),
            "{history:?}"
        );
        if !has_plan(&plan) {
            // What AWR says went wrong while flushing the snapshots.
            let flush_errors = conn
                .query_as::<String>(
                    "select snap_id || ' ' || table_name || ' ORA-' || error_number \
                       from dba_hist_snap_error order by snap_id desc fetch first 10 rows only",
                    &[],
                )
                .map(|rows| rows.flatten().collect::<Vec<_>>());
            panic!("no plan in AWR: {plan:?}\n{history:?}\nsnapshot errors: {flush_errors:?}");
        }
        let newest = history.last().unwrap();
        // A row of the other repository (in a PDB) finds the plan too.
        let (pdb, root): (i64, i64) = conn
            .query_row_as("select con_dbid, dbid from v$database", &[])
            .unwrap();
        let other = if newest.dbid == pdb { root } else { pdb };
        let plan = queries::awr_plan(&conn, &sql_id, newest.plan_hash, other).unwrap();
        assert!(plan.iter().any(|l| l.contains("CONNECT BY")), "{plan:?}");
        conn.execute(
            "begin dbms_workload_repository.remove_colored_sql(:1); end;",
            &[&sql_id],
        )
        .unwrap();
    } else {
        eprintln!("Diagnostics Pack not enabled, skipping plan history");
    }

    if caps.sql_monitor().is_ok() {
        let scope = Scope::Sql(sql_id.clone());
        let mut executions = Vec::new();
        wait_for("the monitored execution", || {
            executions = queries::monitored_executions(&conn, &scope).unwrap();
            !executions.is_empty()
        });
        let e = &executions[0];
        assert_eq!((e.sid, e.serial), (worker_sid, worker_serial));
        assert!(
            !queries::monitored_executions(&conn, &session)
                .unwrap()
                .is_empty()
        );
        let package = caps.sql_monitor_package.unwrap();
        let text = queries::sql_monitor_report(&conn, package, e, false).unwrap();
        assert!(text.contains("SQL Monitoring Report"), "{text}");
        let html = queries::sql_monitor_report(&conn, package, e, true).unwrap();
        assert!(html.to_lowercase().contains("<html"));
    } else {
        eprintln!("Tuning Pack not enabled, skipping SQL Monitor");
    }
}

/// Tablespaces: files and largest segments, or for a temporary one its
/// files; the sessions using the most PGA (#74).
#[test]
fn tablespace_and_pga_drill_down() {
    let Some(conn) = dba() else { return };
    let sysaux = queries::tablespace_detail(&conn, "SYSAUX").unwrap();
    assert!(!sysaux.temporary);
    assert!(
        !sysaux.files.is_empty() && !sysaux.segments.is_empty(),
        "{sysaux:?}"
    );
    assert!(sysaux.segments.windows(2).all(|w| w[0].2 >= w[1].2));
    let temp: String = conn
        .query_row_as(
            "select tablespace_name from dba_tablespaces where contents = 'TEMPORARY' and rownum = 1",
            &[],
        )
        .unwrap();
    let temp = queries::tablespace_detail(&conn, &temp).unwrap();
    assert!(temp.temporary && !temp.files.is_empty(), "{temp:?}");
    let users = queries::pga_users(&conn).unwrap();
    assert!(!users.is_empty());
    assert!(users.windows(2).all(|w| w[0].allocated >= w[1].allocated));
}
