//! SQL Server sessions against a real server.
//!
//! Skipped unless `PYTHIA_TEST_MSSQL` is set to `user/password@host[:port]/database[?trust]`,
//! e.g. the SQL Server container from `docker-compose.yml`:
//! `PYTHIA_TEST_MSSQL=pythia/pythia@localhost:1433/pythia?trust cargo test -p pythia-db --test mssql`
//!
//! The SSH tunnel test also needs `PYTHIA_TEST_SSH=user:password@host[:port]`
//! and `PYTHIA_TEST_MSSQL_VIA_SSH`, the database as the SSH server sees it
//! (the `ssh` service: `pythia:pythia@localhost:2222` and
//! `pythia/pythia@mssql:1433/pythia?trust`).

use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use pythia_db::ssh::{SshParams, Tunnel};
use pythia_db::{
    Bind, ConnectParams, DbEvent, DbKind, DbRequest, Detail, Dialect, MetaQuery, MetaResult,
    ObjectType, Session,
};

fn params() -> Option<ConnectParams> {
    from_spec(&std::env::var("PYTHIA_TEST_MSSQL").ok()?)
}

/// The DBA's login (`PYTHIA_TEST_MSSQL_DBA`), for tests that make
/// databases of their own.
fn dba_params() -> Option<ConnectParams> {
    from_spec(&std::env::var("PYTHIA_TEST_MSSQL_DBA").ok()?)
}

/// The database through the SSH server, trusting the host keys kept in
/// `known_hosts`.
fn ssh_params(known_hosts: &Path) -> Option<ConnectParams> {
    let ssh = std::env::var("PYTHIA_TEST_SSH").ok()?;
    let mut params = from_spec(&std::env::var("PYTHIA_TEST_MSSQL_VIA_SSH").ok()?)?;
    let (credentials, host) = ssh.rsplit_once('@')?;
    let (username, password) = credentials.split_once(':')?;
    params.ssh = Some(Arc::new(Tunnel::new(SshParams {
        host: host.into(),
        username: username.into(),
        password: password.into(),
        key_file: None,
        known_hosts: known_hosts.to_owned(),
    })));
    Some(params)
}

/// `user/password@connect string`.
fn from_spec(spec: &str) -> Option<ConnectParams> {
    let (credentials, connect_string) = spec.rsplit_once('@')?;
    let (username, password) = credentials.split_once('/')?;
    Some(ConnectParams {
        kind: DbKind::SqlServer,
        username: username.into(),
        password: password.into(),
        connect_string: connect_string.into(),
        privilege: Default::default(),
        init_sql: Vec::new(),
        ssh: None,
    })
}

struct Harness {
    session: Session,
    wake: mpsc::Receiver<()>,
    schema: String,
}

impl Harness {
    fn connect() -> Option<Self> {
        Some(Self::with(params()?))
    }

    fn with(params: ConnectParams) -> Self {
        let (tx, wake) = mpsc::channel();
        let session = Session::connect(params, move || {
            let _ = tx.send(());
        });
        let mut h = Self {
            session,
            wake,
            schema: String::new(),
        };
        match h.next() {
            DbEvent::Connected(server) => {
                assert_eq!(server.dialect, Dialect::SQLSERVER);
                assert!(server.release >= (13, 0), "{server:?}");
                assert!(
                    server.banner.starts_with("Microsoft SQL Server"),
                    "{server:?}"
                );
                h.schema = server.current_schema;
                h
            }
            other => panic!("connect failed: {other:?}"),
        }
    }

    fn next(&mut self) -> DbEvent {
        loop {
            if let Some(event) = self.session.try_recv() {
                return event;
            }
            self.wake
                .recv_timeout(Duration::from_secs(60))
                .expect("timed out waiting for db event");
        }
    }

    /// Next event that isn't transaction bookkeeping or a notice.
    fn next_result(&mut self) -> DbEvent {
        loop {
            match self.next() {
                DbEvent::Transaction { .. } | DbEvent::Notice(_) => continue,
                event => return event,
            }
        }
    }

    fn execute(&mut self, id: u64, sql: &str, max_rows: usize) {
        self.session.send(DbRequest::Execute {
            id,
            sql: sql.into(),
            max_rows,
            binds: vec![],
        });
    }

    fn run(&mut self, id: u64, sql: &str) -> DbEvent {
        self.execute(id, sql, 100);
        self.next_result()
    }

    /// Runs a statement that must succeed; returns whether a transaction is
    /// pending after it (reported last).
    fn ok(&mut self, sql: &str) -> bool {
        self.execute(0, sql, 100);
        loop {
            match self.next() {
                DbEvent::Transaction { pending } => return pending,
                DbEvent::Failed { error, .. } => panic!("{sql}: {error}"),
                _ => {}
            }
        }
    }

    /// Runs a statement that must fail; returns whether a transaction is
    /// pending after it.
    fn fails(&mut self, sql: &str) -> bool {
        self.execute(0, sql, 100);
        assert!(
            matches!(self.next_result(), DbEvent::Failed { .. }),
            "{sql} should fail"
        );
        self.settle()
    }

    /// The first value of a query's first row.
    fn scalar(&mut self, sql: &str) -> Option<String> {
        let event = self.run(0, sql);
        let DbEvent::QueryStarted { .. } = event else {
            panic!("{sql}: expected a query, got {event:?}")
        };
        let DbEvent::Rows { rows, .. } = self.next_result() else {
            panic!("{sql}: expected rows")
        };
        self.settle();
        rows[0][0].clone()
    }

    /// A query's first row, NULLs as ''.
    fn first_row(&mut self, sql: &str) -> Vec<String> {
        let event = self.run(0, sql);
        let DbEvent::QueryStarted { .. } = event else {
            panic!("{sql}: expected a query, got {event:?}")
        };
        let DbEvent::Rows { mut rows, .. } = self.next_result() else {
            panic!("{sql}: expected rows")
        };
        self.settle();
        rows.remove(0)
            .into_iter()
            .map(Option::unwrap_or_default)
            .collect()
    }

    /// Reads up to the transaction state that ends a statement's events.
    fn settle(&mut self) -> bool {
        loop {
            if let DbEvent::Transaction { pending } = self.next() {
                return pending;
            }
        }
    }

    fn meta(&mut self, query: MetaQuery) -> MetaResult {
        self.session.send(DbRequest::Meta { id: 99, query });
        loop {
            if let DbEvent::Meta { result, .. } = self.next() {
                return result.expect("meta query");
            }
        }
    }
}

/// `n` numbered rows, without depending on a table.
fn numbers(n: u32) -> String {
    format!(
        "with r (n) as (select 1 union all select n + 1 from r where n < {n}) \
         select n, concat('row ', n) as label from r option (maxrecursion 0)"
    )
}

/// `text` with the names starting with `prefix` made this checkout's own:
/// `r43_ix` becomes `r43_5f3a_ix`. Two checkouts' runs at once that don't
/// share the lock on the test databases (#100), as with a temp directory
/// of their own or from another system, then leave each other's objects
/// alone; a checkout's next run finds what its last one left.
fn own(prefix: &str, text: &str) -> String {
    let checkout = env!("CARGO_MANIFEST_DIR")
        .bytes()
        .fold(0u32, |hash, b| hash.wrapping_mul(31).wrapping_add(b.into()));
    text.replace(prefix, &format!("{prefix}{:04x}_", checkout & 0xffff))
}

#[test]
fn query_and_fetch_more() {
    let Some(mut h) = Harness::connect() else {
        eprintln!("PYTHIA_TEST_MSSQL not set, skipping");
        return;
    };
    h.execute(1, &numbers(25), 10);
    let DbEvent::QueryStarted { columns, .. } = h.next_result() else {
        panic!("expected QueryStarted")
    };
    assert_eq!(
        columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["n", "label"]
    );
    assert!(columns[0].numeric && !columns[1].numeric);
    let DbEvent::Rows { rows, more, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!((rows.len(), more), (10, true));
    assert_eq!(rows[0], vec![Some("1".into()), Some("row 1".into())]);

    h.session.send(DbRequest::FetchMore {
        id: 1,
        max_rows: 100,
    });
    let DbEvent::Rows { rows, more, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!((rows.len(), more), (15, false));
    assert_eq!(rows[14][1].as_deref(), Some("row 25"));
}

#[test]
fn values_types_and_binds() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let DbEvent::QueryStarted { columns, .. } = h.run(
        1,
        "select cast(1.50 as decimal(5,2)) d, cast('2024-02-29' as date) dt, \
                cast('2024-02-29 13:45:01.123' as datetime2(3)) ts, null n, 0x00ff b, \
                N'grüße' s, cast(1 as bit) t, cast('2024-02-29 13:45:01' as datetime) dtm, \
                cast('2024-02-29 13:45:01 +01:30' as datetimeoffset(0)) dto, \
                cast('13:45:01.5' as time(1)) tm, \
                cast('6F9619FF-8B86-D011-B42D-00C04FC964FF' as uniqueidentifier) g, \
                cast(2.5 as float) f",
    ) else {
        panic!("expected QueryStarted")
    };
    let types: Vec<&str> = columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(types[..3], ["decimal", "date", "datetime2"]);
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(
        rows[0],
        vec![
            Some("1.50".into()),
            Some("2024-02-29".into()),
            Some("2024-02-29 13:45:01.123".into()),
            None,
            Some("0x00FF".into()),
            Some("grüße".into()),
            Some("1".into()),
            Some("2024-02-29 13:45:01.000".into()),
            Some("2024-02-29 13:45:01 +01:30".into()),
            Some("13:45:01.5".into()),
            Some("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()),
            Some("2.5".into()),
        ]
    );
    h.settle();

    h.session.send(DbRequest::Execute {
        id: 2,
        sql: "select cast(:a as int) + 1 as x, case when :B is null then 1 else 0 end as y, \
              :a as again"
            .into(),
        max_rows: 10,
        binds: vec![
            ("A".into(), Bind::Text(Some("41".into()))),
            ("b".into(), Bind::Text(None)),
        ],
    });
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(
        rows[0],
        vec![Some("42".into()), Some("1".into()), Some("41".into())]
    );
    h.settle();

    // Variables live in the batch.
    assert_eq!(
        h.scalar("declare @n int = 5;\nselect @n * 2 as x")
            .as_deref(),
        Some("10")
    );
}

/// A float keeps its exponent (#261): 2e-20 came as
/// 0.00000000000000000002, and 1e-300 as 302 characters, which the grid
/// shows cut short as `0.0000…`.
#[test]
fn floats_keep_their_exponent() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let DbEvent::QueryStarted { .. } = h.run(
        1,
        "select cast(2e-20 as float) a, cast(3e15 as float) b, cast(2e20 as float) c, \
                cast(-1e-300 as float) d, cast(2.5e-7 as real) e, cast(2.5 as float) f, \
                cast(100 as float) g, cast(0.0001 as float) h, cast(1e16 as real) i",
    ) else {
        panic!("expected QueryStarted")
    };
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    h.settle();
    let texts: Vec<&str> = rows[0].iter().map(|v| v.as_deref().unwrap()).collect();
    assert_eq!(
        texts,
        [
            "2e-20",
            "3000000000000000",
            "2e20",
            "-1e-300",
            "2.5e-7",
            "2.5",
            "100",
            "0.0001",
            "1e16"
        ]
    );
}

#[test]
fn errors_carry_number_and_line() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let DbEvent::Failed { error, .. } = h.run(1, "select * from no_such_table") else {
        panic!("expected Failed")
    };
    assert_eq!(error.code, Some(208));
    assert_eq!(
        error.message,
        "Msg 208, Level 16, State 1, Line 1\nInvalid object name 'no_such_table'."
    );
    assert!(!error.is_connection_lost());
    h.settle();

    // The line of a syntax error; otherwise the statement's.
    let sql = "select 1\n  from t\n  where and x = 1";
    let DbEvent::Failed { error, .. } = h.run(2, sql) else {
        panic!("expected Failed")
    };
    assert_eq!(error.code, Some(156), "{}", error.message);
    assert_eq!(error.offset, Some(20), "{}", error.message);
}

#[test]
fn transactions_and_autocommit() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists tx_t");
    h.ok("create table tx_t (a int primary key)");
    assert!(h.ok("insert into tx_t values (1)"));
    h.session.send(DbRequest::Rollback);
    assert!(!h.settle());
    assert_eq!(
        h.scalar("select count(*) from tx_t").as_deref(),
        Some("0"),
        "the insert was rolled back"
    );

    // A failed change leaves no transaction behind.
    assert!(!h.fails("insert into tx_t values (5), (5)"));
    // In one, it is undone alone: the transaction goes on.
    assert!(h.ok("insert into tx_t values (5)"));
    assert!(h.fails("insert into tx_t values (5)"));
    assert_eq!(h.scalar("select count(*) from tx_t").as_deref(), Some("1"));
    assert!(h.ok("insert into tx_t values (6)"));
    h.session.send(DbRequest::Commit);
    assert!(!h.settle());
    assert_eq!(h.scalar("select count(*) from tx_t").as_deref(), Some("2"));

    // The server tells: a transaction the batch opens itself is pending.
    assert!(h.ok("begin tran"));
    assert!(!h.ok("commit"));
    // Statistics aren't rows: updating them opens no transaction.
    assert!(!h.ok("update statistics tx_t with fullscan"));

    h.session.send(DbRequest::SetAutocommit(true));
    let DbEvent::Executed { summary, .. } = h.run(2, "insert into tx_t values (2), (3)") else {
        panic!("expected Executed")
    };
    assert_eq!(summary.rows_affected, Some(2));
    assert_eq!(summary.to_string(), "INSERT: 2 rows affected");
    assert!(!h.settle(), "autocommit committed the insert");
    h.ok("drop table tx_t");
}

/// Rows affected are the statement's own, as `@@ROWCOUNT` has them: also
/// with `SET NOCOUNT ON`, which leaves out the counts the driver would
/// read, and with a trigger that changes rows of its own. Editing a result
/// relies on it to tell a conflict.
#[test]
fn updates_count_their_own_rows() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists count_t, count_log");
    h.ok("create table count_t (id int primary key, x int)");
    h.ok("create table count_log (n int)");
    h.ok(
        "create trigger count_tr on count_t after update, delete as \
         begin set nocount on; update count_log set n = n + 1; end",
    );
    h.ok("insert into count_t values (1, 10), (2, 20)");
    h.ok("insert into count_log values (1), (2), (3)");
    let affected = |h: &mut Harness, sql: &str| {
        let DbEvent::Executed { summary, .. } = h.run(1, sql) else {
            panic!("{sql}: expected Executed")
        };
        h.settle();
        summary.rows_affected
    };
    // In the transaction a change opens, as editing runs.
    let matched = affected(&mut h, "update count_t set x = x where id = 1");
    assert_eq!(matched, Some(1));
    let none = affected(&mut h, "update count_t set x = 5 where id = 3");
    assert_eq!(none, Some(0));
    h.ok("set nocount on");
    assert_eq!(affected(&mut h, "update count_t set x = x"), Some(2));
    let deleted = affected(&mut h, "delete from count_t where id = 2");
    assert_eq!(deleted, Some(1));
    h.ok("set nocount off");
    h.session.send(DbRequest::Rollback);
    h.settle();
    h.ok("drop table count_t, count_log");
}

/// Like Oracle's: a query neither keeps a transaction open nor holds locks.
#[test]
fn queries_see_commits_and_lock_nothing() {
    let (Some(mut a), Some(mut b)) = (Harness::connect(), Harness::connect()) else {
        return;
    };
    a.ok("drop table if exists fresh_t");
    a.ok("create table fresh_t (a int)");
    assert_eq!(
        a.scalar("select count(*) from fresh_t").as_deref(),
        Some("0")
    );
    assert!(b.ok("insert into fresh_t values (1)"));
    b.session.send(DbRequest::Commit);
    assert!(!b.settle());
    assert_eq!(
        a.scalar("select count(*) from fresh_t").as_deref(),
        Some("1")
    );
    // `a` read the table; `b` can still change its structure.
    b.ok("alter table fresh_t add b int");
    a.ok("drop table fresh_t");
}

#[test]
fn closing_an_open_result_stops_the_server() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    // A million rows; only ten are fetched. The driver reads the rest.
    let big = "select top 1000000 a.object_id, b.name \
               from sys.all_objects a cross join sys.all_objects b";
    h.execute(1, big, 10);
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));

    // The next statement waits while the driver reads the rest off the
    // wire, as fast as the server sends it, without making rows of it (the
    // server isn't stopped, as MySQL's is): a second or two, ten on a busy
    // machine. The limit is for waiting longer than that.
    let started = Instant::now();
    h.execute(3, "select 'next'", 10);
    assert!(matches!(h.next_result(), DbEvent::QueryClosed { id: 1 }));
    assert!(matches!(
        h.next_result(),
        DbEvent::QueryStarted { id: 3, .. }
    ));
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(rows[0][0].as_deref(), Some("next"));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );

    // Fetching more of the closed result says it is closed.
    h.session.send(DbRequest::FetchMore {
        id: 1,
        max_rows: 10,
    });
    assert!(matches!(h.next_result(), DbEvent::QueryClosed { id: 1 }));

    h.execute(2, &numbers(30), 10);
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));
    h.session.send(DbRequest::Rollback);
    assert!(matches!(h.next_result(), DbEvent::QueryClosed { id: 2 }));

    // The connection is in step.
    for i in 0..3 {
        assert_eq!(h.scalar(&format!("select {i}")), Some(i.to_string()));
    }
}

#[test]
fn cancel_interrupts_a_running_statement() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists cancel_t");
    h.ok("create table cancel_t (a int)");
    assert!(h.ok("insert into cancel_t values (1)"));
    h.execute(1, "waitfor delay '00:00:30'; select 1", 10);
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    h.session.cancel();
    let mut notices = Vec::new();
    let error = loop {
        match h.next() {
            DbEvent::Failed { error, .. } => break error,
            DbEvent::Notice(n) => notices.push(n),
            _ => {}
        }
    };
    assert_eq!(error.message, "Cancelled");
    assert!(started.elapsed() < Duration::from_secs(5));
    // The driver needs a new connection after a cancel; the insert is gone.
    loop {
        match h.next() {
            DbEvent::Notice(n) => notices.push(n),
            DbEvent::Transaction { pending } => {
                assert!(!pending);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        notices,
        [
            "After a cancel, SQL Server's driver can't go on: the connection was opened again; \
             changes that weren't committed were rolled back"
        ]
    );
    assert_eq!(
        h.scalar("select count(*) from cancel_t").as_deref(),
        Some("0")
    );
    h.ok("drop table cancel_t");

    // A Break while nothing runs doesn't stop the next statement.
    h.session.cancel();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.scalar("select 'later'").as_deref(), Some("later"));
}

/// A statement takes as long as it takes: past 30 s the driver gave up
/// waiting for the server, and the connection was lost with what wasn't
/// committed (#199).
#[test]
fn long_statements_keep_the_transaction() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists fx_long_t");
    h.ok("create table fx_long_t (a int)");
    assert!(h.ok("insert into fx_long_t values (1)"));
    let event = h.run(1, "waitfor delay '00:00:31'; select 1");
    assert!(matches!(event, DbEvent::QueryStarted { .. }), "{event:?}");
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(rows[0][0].as_deref(), Some("1"));
    assert!(h.settle(), "the insert is still pending");
    assert_eq!(
        h.scalar("select count(*) from fx_long_t").as_deref(),
        Some("1")
    );
    h.session.send(DbRequest::Rollback);
    assert!(!h.settle());
    h.ok("drop table fx_long_t");
}

/// The events of statement `id` up to the transaction state that ends
/// them, or up to rows that wait for a FetchMore.
fn outline(h: &mut Harness, id: u64) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        out.push(match h.next() {
            DbEvent::Transaction { .. } => return out,
            DbEvent::Notice(_) => continue,
            DbEvent::QueryStarted { id: of, columns } if of == id => {
                let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
                format!("cols {}", names.join(" "))
            }
            DbEvent::Rows {
                id: of, rows, more, ..
            } if of == id => format!("rows {} {more}", rows.len()),
            DbEvent::Failed { id: of, error, .. } if of == id => {
                format!("failed {}", error.code.unwrap_or_default())
            }
            other => format!("{other:?}"),
        });
        // Waiting for a FetchMore: no transaction state comes yet.
        if out.last().is_some_and(|e| e.ends_with(" true")) {
            return out;
        }
    }
}

/// Every result set of a batch is a result (#316): a DECLARE batch's
/// SELECTs, a procedure's. Only the first showed, with a notice, and an
/// error after it didn't fail the statement.
#[test]
fn several_result_sets() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let id = 66;
    let run = |h: &mut Harness, sql: &str, max_rows: usize| {
        h.execute(id, sql, max_rows);
        outline(h, id)
    };
    let more = "MoreResults { id: 66 }";
    assert_eq!(
        run(&mut h, "select 1 as a; select 2 as b; select 3 as c", 10),
        [
            "cols a",
            more,
            "rows 1 false",
            "cols b",
            more,
            "rows 1 false",
            "cols c",
            "rows 1 false"
        ]
    );
    assert_eq!(
        run(
            &mut h,
            "declare @n int = 1; select @n as a; select @n + 1 as b, 'x' as c",
            10
        ),
        ["cols a", more, "rows 1 false", "cols b c", "rows 1 false"]
    );
    // An empty result is one, a statement without rows isn't.
    assert_eq!(
        run(
            &mut h,
            "select 1 as a where 1 = 0; declare @x int; set @x = 1; select @x as x",
            10
        ),
        ["cols a", more, "rows 0 false", "cols x", "rows 1 false"]
    );
    // An error after a result fails the batch, also one in the next
    // result, which has its columns but no rows.
    assert_eq!(
        run(&mut h, "select 1 as a; select 1 / 0 as b", 10),
        [
            "cols a",
            more,
            "rows 1 false",
            "cols b",
            more,
            "rows 0 false",
            "failed 8134"
        ]
    );
    assert_eq!(
        run(&mut h, "select 1 as a; raiserror('r66 after', 16, 1)", 10),
        ["cols a", more, "rows 1 false", "failed 50000"]
    );
    // A result fetched batch by batch: the next one comes after its last.
    let paged = format!("{}; select 'end' as e", numbers(15));
    assert_eq!(run(&mut h, &paged, 10), ["cols n label", "rows 10 true"]);
    h.session.send(DbRequest::FetchMore { id, max_rows: 10 });
    assert_eq!(
        outline(&mut h, id),
        [more, "rows 5 false", "cols e", "rows 1 false"]
    );
    // Fetch all of the first doesn't fetch all of the next.
    let twice = format!("{0}; {0}", numbers(15));
    assert_eq!(run(&mut h, &twice, 10), ["cols n label", "rows 10 true"]);
    h.session.send(DbRequest::FetchMore {
        id,
        max_rows: usize::MAX,
    });
    assert_eq!(
        outline(&mut h, id),
        [more, "rows 5 false", "cols n label", "rows 10 true"]
    );
    h.session.send(DbRequest::Rollback);
    let mut closed = false;
    loop {
        match h.next() {
            DbEvent::QueryClosed { id: 66 } => closed = true,
            DbEvent::Notice(n) if n == "Rolled back" => break,
            _ => {}
        }
    }
    assert!(closed);
    h.settle();
    // One result, as before.
    assert_eq!(run(&mut h, "select 1 as a", 10), ["cols a", "rows 1 false"]);
    // A file has the first, and the others are said.
    h.session.send(DbRequest::ExecuteWhole {
        id,
        sql: "select 1 as a; select 2 as b; select 3 as c".into(),
        max_rows: 10,
    });
    let mut notices = Vec::new();
    let mut shown = Vec::new();
    loop {
        match h.next() {
            DbEvent::Transaction { .. } => break,
            DbEvent::Notice(text) => notices.push(text),
            DbEvent::QueryStarted { .. } => shown.push("cols"),
            DbEvent::Rows { .. } => shown.push("rows"),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(shown, ["cols", "rows"]);
    assert_eq!(
        notices,
        ["The batch returned 2 more result set(s); only the first was written to the file"]
    );
}

/// PRINT output and informational messages show as messages, in order
/// with the result: before its columns, after its rows (#98).
#[test]
fn print_and_info_messages() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let events = |h: &mut Harness, sql: &str| {
        h.execute(1, sql, 10);
        let mut seen = Vec::new();
        loop {
            match h.next() {
                DbEvent::QueryStarted { .. } => seen.push("(columns)".to_owned()),
                DbEvent::Rows { .. } => seen.push("(rows)".to_owned()),
                DbEvent::Notice(n) => seen.push(n),
                DbEvent::Failed { error, .. } => seen.push(format!("(failed: {})", error.message)),
                DbEvent::Transaction { .. } => return seen,
                _ => {}
            }
        }
    };
    assert_eq!(
        events(
            &mut h,
            "print 'before'; select 1 as a; raiserror('after', 10, 1) with nowait"
        ),
        ["before", "(columns)", "(rows)", "after"]
    );
    // No result at all; and messages before an error.
    assert_eq!(events(&mut h, "print 'only a message'"), ["only a message"]);
    let failed = events(&mut h, "print 'first'; select 1 / 0");
    assert_eq!(failed[0], "first");
    assert!(
        failed.iter().any(|e| e.contains("Divide by zero")),
        "{failed:?}"
    );
}

#[test]
fn oracle_tasks_are_refused() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::Task {
        id: 5,
        task: Box::new(|_| Ok(Box::new(()))),
    });
    let DbEvent::TaskDone { id, result } = h.next_result() else {
        panic!("expected TaskDone")
    };
    assert_eq!(id, 5);
    let error = result.expect_err("an error");
    assert_eq!(
        error.message,
        "This tool isn't available for SQL Server connections"
    );
}

#[test]
fn dictionary() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let schema = h.schema.clone();
    h.session.send(DbRequest::SetAutocommit(true));
    let n = |text: &str| own("meta_", text);
    h.ok(&n("drop view if exists meta_v"));
    h.ok(&n("drop procedure if exists meta_p"));
    h.ok(&n("drop table if exists meta_emp"));
    h.ok(&n("drop table if exists meta_dept"));
    h.ok(&n(
        "create table meta_dept (deptno int primary key, name varchar(30) not null)",
    ));
    h.ok(&n(
        "exec sp_addextendedproperty 'MS_Description', 'Department', 'schema', 'dbo', \
         'table', 'meta_dept', 'column', 'name'",
    ));
    h.ok(&n(
        "create table meta_emp (empno int identity(1, 1) primary key, \
         deptno int constraint meta_emp_fk references meta_dept (deptno), \
         [Salary] decimal(8,2) not null constraint meta_emp_sal default 0, \
         note nvarchar(max))",
    ));
    h.ok(&n("create index meta_emp_dept on meta_emp (deptno)"));
    h.ok(&n("create view meta_v as select e.empno, d.name from dbo.meta_emp e join dbo.meta_dept d on d.deptno = e.deptno"));
    h.ok(&n("create procedure meta_p @p int as begin select @p; end"));

    let MetaResult::Schemas(schemas) = h.meta(MetaQuery::Schemas) else {
        panic!("expected schemas")
    };
    assert!(schemas.contains(&schema), "{schemas:?}");
    assert!(!schemas.contains(&"db_owner".to_owned()), "{schemas:?}");

    let MetaResult::Objects(tables) = h.meta(MetaQuery::Objects {
        owner: schema.clone(),
        object_type: ObjectType::Table,
        filter: Some(n("meta_").to_uppercase()),
    }) else {
        panic!("expected objects")
    };
    let names: Vec<&str> = tables.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, [n("meta_dept"), n("meta_emp")]);
    assert_eq!(tables[0].last_ddl.len(), 19, "{:?}", tables[0]);

    let MetaResult::Objects(procedures) = h.meta(MetaQuery::Objects {
        owner: schema.clone(),
        object_type: ObjectType::Procedure,
        filter: Some(n("meta_")),
    }) else {
        panic!("expected objects")
    };
    assert_eq!(procedures.len(), 1, "{procedures:?}");

    // Code assistance asks in uppercase.
    let MetaResult::Columns(columns) = h.meta(MetaQuery::Columns {
        owner: schema.clone(),
        table: n("meta_dept").to_uppercase(),
        keys: true,
    }) else {
        panic!("expected columns")
    };
    assert_eq!(columns.len(), 2);
    assert_eq!(
        (columns[0].name.as_str(), columns[0].pk_position),
        ("deptno", Some(1))
    );
    assert_eq!(columns[1].data_type, "varchar(30)");
    assert!(!columns[1].nullable);
    assert_eq!(columns[1].comment.as_deref(), Some("Department"));

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: n("meta_emp"),
        detail: Detail::Constraints,
    }) else {
        panic!("expected a table")
    };
    let fk = rows
        .iter()
        .find(|r| r[0].as_deref() == Some(n("meta_emp_fk").as_str()))
        .expect("the foreign key");
    assert_eq!(fk[1].as_deref(), Some("Foreign key"));
    assert_eq!(fk[3].as_deref(), Some(n("dbo.meta_dept").as_str()));

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: n("meta_emp"),
        detail: Detail::Indexes,
    }) else {
        panic!("expected a table")
    };
    let index = rows
        .iter()
        .find(|r| r[0].as_deref() == Some(n("meta_emp_dept").as_str()))
        .expect("the index");
    assert_eq!(index[3].as_deref(), Some("deptno"));

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: n("meta_dept"),
        detail: Detail::UsedBy,
    }) else {
        panic!("expected a table")
    };
    let used_by: Vec<_> = rows
        .iter()
        .map(|r| r[1].clone().unwrap_or_default())
        .collect();
    assert!(used_by.contains(&n("meta_v")), "{used_by:?}");
    assert!(used_by.contains(&n("meta_emp")), "{used_by:?}");

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: n("meta_v"),
        detail: Detail::Uses,
    }) else {
        panic!("expected a table")
    };
    assert_eq!(rows.len(), 2, "{rows:?}");

    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: schema.clone(),
        name: n("meta_emp"),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    assert!(
        ddl.starts_with(&n(&format!(
            "create table [{schema}].[meta_emp] (\n    [empno] int identity(1, 1) not null,"
        ))),
        "{ddl}"
    );
    assert!(
        ddl.contains(&n(
            "[Salary] decimal(8,2) not null constraint [meta_emp_sal] default ((0))"
        )),
        "{ddl}"
    );
    assert!(ddl.contains("[note] nvarchar(max) null"), "{ddl}");
    assert!(
        ddl.contains(&n(
            "constraint [meta_emp_fk] foreign key ([deptno]) references [dbo].[meta_dept] ([deptno])"
        )),
        "{ddl}"
    );
    assert!(
        ddl.contains(&n("create nonclustered index [meta_emp_dept] on")),
        "{ddl}"
    );
    assert!(ddl.ends_with("\nGO\n"), "{ddl}");
    // The DDL runs: it rebuilds the table elsewhere.
    h.ok(&n(
        "if schema_id('meta_copy') is null exec('create schema meta_copy')",
    ));
    for statement in pythia_plsql::split_statements_in(&ddl, pythia_plsql::Syntax::TSql) {
        let sql = statement
            .text(&ddl)
            .replace(
                &n(&format!("[{schema}].[meta_emp]")),
                &n("[meta_copy].[meta_emp]"),
            )
            .replace(&n("[meta_emp_sal]"), &n("[meta_copy_sal]"))
            .replace(&n("[meta_emp_fk]"), &n("[meta_copy_fk]"))
            .replace("[PK__", "[PK_copy__");
        h.ok(&sql);
    }
    h.ok(&n("drop table meta_copy.meta_emp"));
    h.ok(&n("drop schema meta_copy"));

    let MetaResult::Text(source) = h.meta(MetaQuery::Source {
        owner: schema.clone(),
        name: n("meta_p"),
        object_type: ObjectType::Procedure,
    }) else {
        panic!("expected source")
    };
    assert_eq!(
        source,
        n("create procedure meta_p @p int as begin select @p; end")
    );

    let MetaResult::Objects(found) = h.meta(MetaQuery::Find {
        pattern: n("meta_*"),
        limit: 10,
    }) else {
        panic!("expected objects")
    };
    assert!(
        found
            .iter()
            .any(|o| o.name == n("meta_p") && o.object_type == ObjectType::Procedure)
    );
    assert_eq!(found[0].owner, schema, "the default schema comes first");

    let MetaResult::Table { columns, rows } = h.meta(MetaQuery::ExplainPlan {
        sql: n(
            "select * from meta_emp e join meta_dept d on d.deptno = e.deptno \
                where e.empno = :id",
        ),
    }) else {
        panic!("expected a plan")
    };
    assert_eq!(columns[0].name, "ID");
    assert_eq!(rows[0][3].as_deref(), Some("SELECT"), "{rows:?}");
    assert!(
        rows.iter().any(|r| r[4]
            .as_deref()
            .is_some_and(|o| o.starts_with(&n("dbo.meta_")))),
        "{rows:?}"
    );
    // SHOWPLAN is off again: statements run.
    assert_eq!(h.scalar("select 'runs'").as_deref(), Some("runs"));

    h.ok(&n("drop view meta_v"));
    h.ok(&n("drop procedure meta_p"));
    h.ok(&n("drop table meta_emp"));
    h.ok(&n("drop table meta_dept"));
}

/// A table's indexes of the kinds CREATE INDEX makes: covering, filtered,
/// unique, descending and columnstore. *Indexes* has each one's columns
/// and INCLUDE columns, and the table's DDL creates each of them again.
#[test]
fn indexes_of_every_kind() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    // What the dictionary reads is compared with the names as written.
    let n = |text: &str| own("r43_", text);
    let written = |text: String| text.replace(&n("r43_"), "r43_");
    h.ok(&n("drop table if exists r43_copy.r43_ix, r43_copy.r43_cs"));
    h.ok(&n("drop schema if exists r43_copy"));
    h.ok(&n("drop table if exists r43_ix, r43_cs"));
    for sql in [
        "create table r43_ix (id int constraint r43_ix_pk primary key, a int, b varchar(10), \
         c int not null)",
        "create index r43_ix_cover on r43_ix (a desc) include (c, b)",
        "create index r43_ix_some on r43_ix (b) where b is not null",
        "create unique index r43_ix_c on r43_ix (c)",
        "create nonclustered columnstore index r43_ix_store on r43_ix (b, a)",
        "create table r43_cs (a int, b int)",
        "create clustered columnstore index r43_cs_store on r43_cs",
        "create index r43_cs_b on r43_cs (b)",
    ] {
        h.ok(&n(sql));
    }
    let indexes = |h: &mut Harness, owner: &str, table: &str| -> Vec<Vec<String>> {
        let MetaResult::Table { columns, rows } = h.meta(MetaQuery::Details {
            owner: n(owner),
            name: n(table),
            detail: Detail::Indexes,
        }) else {
            panic!("expected a table")
        };
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "INDEX_NAME",
                "INDEX_TYPE",
                "UNIQUENESS",
                "COLUMNS",
                "INCLUDE",
                "WHERE"
            ]
        );
        rows.into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| written(v.unwrap_or_default()))
                    .collect()
            })
            .collect()
    };
    let ix = indexes(&mut h, &schema, "r43_ix");
    assert_eq!(
        ix,
        [
            ["r43_ix_pk", "CLUSTERED", "PRIMARY", "id", "", ""],
            ["r43_ix_c", "NONCLUSTERED", "UNIQUE", "c", "", ""],
            [
                "r43_ix_cover",
                "NONCLUSTERED",
                "NONUNIQUE",
                "a DESC",
                "c, b",
                ""
            ],
            [
                "r43_ix_some",
                "NONCLUSTERED",
                "NONUNIQUE",
                "b",
                "",
                "([b] IS NOT NULL)"
            ],
            [
                "r43_ix_store",
                "NONCLUSTERED COLUMNSTORE",
                "NONUNIQUE",
                "b, a",
                "",
                ""
            ],
        ]
    );
    let cs = indexes(&mut h, &schema, "r43_cs");
    assert_eq!(
        cs,
        [
            ["r43_cs_b", "NONCLUSTERED", "NONUNIQUE", "b", "", ""],
            [
                "r43_cs_store",
                "CLUSTERED COLUMNSTORE",
                "NONUNIQUE",
                "a, b",
                "",
                ""
            ],
        ]
    );

    let ddl = |h: &mut Harness, table: &str| {
        let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
            owner: schema.clone(),
            name: n(table),
            object_type: ObjectType::Table,
        }) else {
            panic!("expected DDL")
        };
        written(ddl)
    };
    let ix_ddl = ddl(&mut h, "r43_ix");
    for line in [
        format!(
            "create nonclustered index [r43_ix_cover] on [{schema}].[r43_ix] ([a] desc) include ([c], [b]);"
        ),
        format!(
            "create nonclustered index [r43_ix_some] on [{schema}].[r43_ix] ([b]) where ([b] IS NOT NULL);"
        ),
        format!("create unique nonclustered index [r43_ix_c] on [{schema}].[r43_ix] ([c]);"),
        format!(
            "create nonclustered columnstore index [r43_ix_store] on [{schema}].[r43_ix] ([b], [a]);"
        ),
    ] {
        assert!(ix_ddl.contains(&line), "{line}\n{ix_ddl}");
    }
    let cs_ddl = ddl(&mut h, "r43_cs");
    let line = format!("create clustered columnstore index [r43_cs_store] on [{schema}].[r43_cs];");
    assert!(cs_ddl.contains(&line), "{line}\n{cs_ddl}");
    // The DDL runs: the tables built from it elsewhere have the same
    // indexes.
    h.ok(&n("create schema r43_copy"));
    for (table, ddl) in [("r43_ix", &ix_ddl), ("r43_cs", &cs_ddl)] {
        for statement in pythia_plsql::split_statements_in(ddl, pythia_plsql::Syntax::TSql) {
            let sql = statement.text(ddl).replace(
                &format!("[{schema}].[{table}]"),
                &format!("[r43_copy].[{table}]"),
            );
            h.ok(&n(&sql));
        }
    }
    assert_eq!(indexes(&mut h, "r43_copy", "r43_ix"), ix);
    assert_eq!(indexes(&mut h, "r43_copy", "r43_cs"), cs);
    h.ok(&n("drop table r43_copy.r43_ix, r43_copy.r43_cs"));
    h.ok(&n("drop schema r43_copy"));
    h.ok(&n("drop table r43_ix, r43_cs"));
}

/// A table's DDL creates its XML and spatial indexes, with their options,
/// and an ordered columnstore index with its ORDER (SQL Server 2022): run
/// elsewhere, the copy has the same indexes.
#[test]
fn xml_spatial_and_ordered_indexes() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    let ordered = h
        .scalar("select cast(serverproperty('ProductMajorVersion') as int)")
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|major| major >= 16);
    // What the dictionary reads is compared with the names as written.
    let n = |text: &str| own("r53_", text);
    let written = |text: String| text.replace(&n("r53_"), "r53_");
    h.ok(&n("drop table if exists r53_copy.r53_xg, r53_copy.r53_ord"));
    h.ok(&n("drop schema if exists r53_copy"));
    h.ok(&n("drop table if exists r53_xg, r53_ord"));
    for sql in [
        "create table r53_xg (id int constraint r53_xg_pk primary key, doc xml, \
         shape geometry, place geography)",
        "create primary xml index r53_xg_doc on r53_xg (doc)",
        "create xml index r53_xg_path on r53_xg (doc) using xml index r53_xg_doc for path",
        "create xml index r53_xg_value on r53_xg (doc) using xml index r53_xg_doc for value",
        "create spatial index r53_xg_auto on r53_xg (shape) using geometry_auto_grid \
         with (bounding_box = (0, -0.5, 1e7, 50.25), cells_per_object = 12)",
        "create spatial index r53_xg_grid on r53_xg (shape) using geometry_grid \
         with (bounding_box = (xmin = -10, ymin = -10, xmax = 10, ymax = 10), \
         grids = (level_1 = low, level_2 = medium, level_3 = high, level_4 = medium))",
        "create spatial index r53_xg_place on r53_xg (place)",
        "create table r53_ord (a int, b int, c int)",
    ] {
        h.ok(&n(sql));
    }
    h.ok(&n(if ordered {
        "create clustered columnstore index r53_ord_cs on r53_ord order (b, a)"
    } else {
        "create clustered columnstore index r53_ord_cs on r53_ord"
    }));
    let ddl = |h: &mut Harness, table: &str| {
        let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
            owner: schema.clone(),
            name: n(table),
            object_type: ObjectType::Table,
        }) else {
            panic!("expected DDL")
        };
        written(ddl)
    };
    let xg_ddl = ddl(&mut h, "r53_xg");
    let on = format!("on [{schema}].[r53_xg]");
    for line in [
        format!("create primary xml index [r53_xg_doc] {on} ([doc]);"),
        format!(
            "create xml index [r53_xg_path] {on} ([doc]) using xml index [r53_xg_doc] for path;"
        ),
        format!(
            "create spatial index [r53_xg_auto] {on} ([shape]) using geometry_auto_grid \
             with (bounding_box = (0, -0.5, 10000000, 50.25), cells_per_object = 12);"
        ),
        format!(
            "create spatial index [r53_xg_grid] {on} ([shape]) using geometry_grid \
             with (bounding_box = (-10, -10, 10, 10), grids = (level_1 = low, \
             level_2 = medium, level_3 = high, level_4 = medium), cells_per_object = 16);"
        ),
        format!(
            "create spatial index [r53_xg_place] {on} ([place]) using geography_auto_grid \
             with (cells_per_object = 12);"
        ),
    ] {
        assert!(xg_ddl.contains(&line), "{line}\n{xg_ddl}");
    }
    let ord_ddl = ddl(&mut h, "r53_ord");
    let line = format!(
        "create clustered columnstore index [r53_ord_cs] on [{schema}].[r53_ord]{};",
        if ordered { " order ([b], [a])" } else { "" }
    );
    assert!(ord_ddl.contains(&line), "{line}\n{ord_ddl}");
    // The indexes as the catalog has them, built again from the DDL.
    let indexes = |h: &mut Harness, owner: &str| {
        h.scalar(&n(&format!(
            "select string_agg(concat_ws(' ', i.name, i.type_desc, x.secondary_type_desc, \
                                         s.tessellation_scheme, t.bounding_box_xmin, \
                                         t.bounding_box_ymin, t.bounding_box_xmax, \
                                         t.bounding_box_ymax, t.level_1_grid_desc, \
                                         t.level_4_grid_desc, t.cells_per_object) \
                                collate database_default, '; ') \
                      within group (order by i.name) \
               from sys.indexes i \
               left join sys.xml_indexes x on x.object_id = i.object_id and x.index_id = i.index_id \
               left join sys.spatial_indexes s on s.object_id = i.object_id \
                and s.index_id = i.index_id \
               left join sys.spatial_index_tessellations t on t.object_id = i.object_id \
                and t.index_id = i.index_id \
              where i.object_id in (object_id('[{owner}].[r53_xg]'), object_id('[{owner}].[r53_ord]'))"
        )))
        .map(written)
    };
    let before = indexes(&mut h, &schema);
    h.ok(&n("create schema r53_copy"));
    for (table, ddl) in [("r53_xg", &xg_ddl), ("r53_ord", &ord_ddl)] {
        for statement in pythia_plsql::split_statements_in(ddl, pythia_plsql::Syntax::TSql) {
            let sql = statement.text(ddl).replace(
                &format!("[{schema}].[{table}]"),
                &format!("[r53_copy].[{table}]"),
            );
            h.ok(&n(&sql));
        }
    }
    assert_eq!(indexes(&mut h, "r53_copy"), before);
    if ordered {
        assert_eq!(
            h.scalar(&n(
                "select string_agg(col_name(object_id, column_id), ', ') \
                          within group (order by column_store_order_ordinal) \
                   from sys.index_columns \
                  where object_id = object_id('r53_copy.r53_ord') and column_store_order_ordinal > 0"
            ))
            .as_deref(),
            Some("b, a")
        );
    }
    h.ok(&n("drop table r53_copy.r53_xg, r53_copy.r53_ord"));
    h.ok(&n("drop schema r53_copy"));
    h.ok(&n("drop table r53_xg, r53_ord"));
}

/// A database of the test's own, made afresh by `dba` and set up with
/// `setup` (`{db}` is its name); the harness is connected to it, in
/// autocommit.
fn own_database(dba: &mut Harness, name: &str, setup: &[&str]) -> Harness {
    drop_database(dba, name);
    dba.ok(&format!("create database [{name}]"));
    for sql in setup {
        dba.ok(&sql.replace("{db}", name));
    }
    let mut params = dba_params().expect("the DBA's login");
    let (server, rest) = params.connect_string.rsplit_once('/').expect("a database");
    let options = rest.find('?').map_or("", |i| &rest[i..]);
    params.connect_string = format!("{server}/{name}{options}");
    let h = Harness::with(params);
    h.session.send(DbRequest::SetAutocommit(true));
    h
}

/// Drops a database `own_database` made, and the sessions still in it.
fn drop_database(dba: &mut Harness, name: &str) {
    dba.ok(&format!(
        "if db_id('{name}') is not null begin \
           alter database [{name}] set single_user with rollback immediate; \
           drop database [{name}]; \
         end"
    ));
}

/// A table's details and DDL as lines: Indexes, Constraints and Triggers,
/// each with its columns' names first, then the DDL.
fn table_dictionary(h: &mut Harness, owner: &str, table: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for detail in [Detail::Indexes, Detail::Constraints, Detail::Triggers] {
        let MetaResult::Table { columns, rows } = h.meta(MetaQuery::Details {
            owner: owner.into(),
            name: table.into(),
            detail,
        }) else {
            panic!("expected a table")
        };
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        lines.push(names.join(" | "));
        for row in rows {
            let values: Vec<String> = row.into_iter().map(Option::unwrap_or_default).collect();
            lines.push(values.join(" | "));
        }
    }
    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: owner.into(),
        name: table.into(),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    lines.extend(ddl.lines().map(str::to_owned));
    lines
}

/// Indexes, Constraints, Triggers and a table's DDL in a database at
/// compatibility level 100, as one from SQL Server 2008 is, read as at
/// 160 (#294): there string_agg's WITHIN GROUP is a syntax error, and SQL
/// Server 2016 has no string_agg at all.
#[test]
fn dictionary_at_compatibility_level_100() {
    let Some(params) = dba_params() else {
        return;
    };
    let mut dba = Harness::with(params);
    dba.session.send(DbRequest::SetAutocommit(true));
    let mut h = own_database(&mut dba, "r63_compat", &[]);
    for sql in [
        "create table r63_dept (deptno int constraint r63_dept_pk primary key, \
         site int not null, name varchar(30) not null, \
         constraint r63_dept_site unique (site desc, deptno))",
        "create table r63_emp (empno int constraint r63_emp_pk primary key nonclustered, \
         deptno int, site int, sal decimal(8,2) constraint r63_emp_sal default 0, \
         [na,me] varchar(30), \
         constraint r63_emp_fk foreign key (site, deptno) references r63_dept (site, deptno) \
           on delete cascade, \
         constraint r63_emp_sal_ck check (sal >= 0))",
        "create clustered index r63_emp_name on r63_emp ([na,me], empno desc)",
        "create index r63_emp_cover on r63_emp (deptno) include (sal, [na,me]) \
         where deptno is not null",
        "create nonclustered columnstore index r63_emp_store on r63_emp (sal, site)",
        "create trigger r63_emp_tr on r63_emp after delete, insert, update as set nocount on",
        "create trigger r63_emp_ins on r63_emp after insert as set nocount on",
    ] {
        h.ok(sql);
    }
    let read = |h: &mut Harness| {
        let mut lines = table_dictionary(h, "dbo", "r63_dept");
        lines.extend(table_dictionary(h, "dbo", "r63_emp"));
        lines
    };
    // As string_agg made them.
    let expected = [
        "INDEX_NAME | INDEX_TYPE | UNIQUENESS | COLUMNS | INCLUDE | WHERE",
        "r63_dept_pk | CLUSTERED | PRIMARY | deptno |  | ",
        "r63_dept_site | NONCLUSTERED | UNIQUE | site DESC, deptno |  | ",
        "CONSTRAINT_NAME | TYPE | DEFINITION | REFERENCES",
        "r63_dept_pk | Primary key | deptno | ",
        "r63_dept_site | Unique | site, deptno | ",
        "TRIGGER_NAME | TRIGGERING_EVENT | TIMING | STATUS",
        "create table [dbo].[r63_dept] (",
        "    [deptno] int not null,",
        "    [site] int not null,",
        "    [name] varchar(30) not null,",
        "    constraint [r63_dept_pk] primary key clustered ([deptno]),",
        "    constraint [r63_dept_site] unique nonclustered ([site] desc, [deptno])",
        ");",
        "GO",
        "INDEX_NAME | INDEX_TYPE | UNIQUENESS | COLUMNS | INCLUDE | WHERE",
        "r63_emp_pk | NONCLUSTERED | PRIMARY | empno |  | ",
        "r63_emp_cover | NONCLUSTERED | NONUNIQUE | deptno | sal, na,me | ([deptno] IS NOT NULL)",
        "r63_emp_name | CLUSTERED | NONUNIQUE | na,me, empno DESC |  | ",
        "r63_emp_store | NONCLUSTERED COLUMNSTORE | NONUNIQUE | sal, site |  | ",
        "CONSTRAINT_NAME | TYPE | DEFINITION | REFERENCES",
        "r63_emp_pk | Primary key | empno | ",
        "r63_emp_fk | Foreign key | site, deptno | dbo.r63_dept",
        "r63_emp_sal_ck | Check | ([sal]>=(0)) | ",
        "r63_emp_sal | Default | sal ((0)) | ",
        "TRIGGER_NAME | TRIGGERING_EVENT | TIMING | STATUS",
        "r63_emp_ins | INSERT | AFTER | ENABLED",
        "r63_emp_tr | INSERT, UPDATE, DELETE | AFTER | ENABLED",
        "create table [dbo].[r63_emp] (",
        "    [empno] int not null,",
        "    [deptno] int null,",
        "    [site] int null,",
        "    [sal] decimal(8,2) null constraint [r63_emp_sal] default ((0)),",
        "    [na,me] varchar(30) null,",
        "    constraint [r63_emp_pk] primary key nonclustered ([empno]),",
        "    constraint [r63_emp_fk] foreign key ([site], [deptno]) references [dbo].[r63_dept] \
         ([site], [deptno]) on delete cascade,",
        "    constraint [r63_emp_sal_ck] check ([sal]>=(0))",
        ");",
        "",
        "create clustered index [r63_emp_name] on [dbo].[r63_emp] ([na,me], [empno] desc);",
        "",
        "create nonclustered index [r63_emp_cover] on [dbo].[r63_emp] ([deptno]) \
         include ([sal], [na,me]) where ([deptno] IS NOT NULL);",
        "",
        "create nonclustered columnstore index [r63_emp_store] on [dbo].[r63_emp] \
         ([sal], [site]);",
        "GO",
    ];
    for level in [160, 100] {
        h.ok(&format!(
            "alter database current set compatibility_level = {level}"
        ));
        assert_eq!(read(&mut h), expected, "at {level}");
    }
    drop(h);
    drop_database(&mut dba, "r63_compat");
}

/// The indexes of `tables` in `owner` as the catalog has them, and whether
/// each table is memory-optimized and how durable.
fn catalog_indexes(h: &mut Harness, owner: &str, tables: &[&str]) -> Vec<String> {
    let names: Vec<String> = tables.iter().map(|t| format!("'{t}'")).collect();
    let names = names.join(", ");
    let tables = h.scalar(&format!(
        "select string_agg(concat_ws(' ', t.name, t.is_memory_optimized, \
                                     t.durability_desc collate database_default), '; ') \
                  within group (order by t.name) \
           from sys.tables t where t.schema_id = schema_id('{owner}') and t.name in ({names})"
    ));
    let indexes = h.scalar(&format!(
        "select string_agg(x.line collate database_default, '; ') within group (order by x.line) \
           from (select concat_ws(' ', object_name(i.object_id), i.name, \
                                  i.type_desc collate database_default, i.is_unique, \
                                  i.is_primary_key, i.is_unique_constraint, h.bucket_count, \
                                  c.columns) as line \
                   from sys.indexes i \
                   left join sys.hash_indexes h on h.object_id = i.object_id \
                    and h.index_id = i.index_id \
                  outer apply (select string_agg(col_name(ic.object_id, ic.column_id) \
                                                 + case when ic.is_descending_key = 1 \
                                                        then ' desc' else '' end, ',') \
                                        within group (order by ic.key_ordinal, \
                                                               ic.index_column_id) as columns \
                                 from sys.index_columns ic \
                                where ic.object_id = i.object_id \
                                  and ic.index_id = i.index_id) c \
                  where i.object_id in (select object_id from sys.tables \
                                         where schema_id = schema_id('{owner}') \
                                           and name in ({names}))) x"
    ));
    [tables, indexes].into_iter().flatten().collect()
}

/// Runs a table's DDL with the tables `tables` of `schema` named in `copy`.
fn run_ddl_in(h: &mut Harness, ddl: &str, schema: &str, copy: &str, tables: &[&str]) {
    for statement in pythia_plsql::split_statements_in(ddl, pythia_plsql::Syntax::TSql) {
        let mut sql = statement.text(ddl).to_owned();
        for table in tables {
            sql = sql.replace(
                &format!("[{schema}].[{table}]"),
                &format!("[{copy}].[{table}]"),
            );
        }
        h.ok(&sql);
    }
}

/// A memory-optimized table's DDL makes it memory-optimized again, as
/// durable, with its hash indexes and their bucket counts, and with the
/// indexes declared in it: CREATE INDEX can't make them (#302).
#[test]
fn memory_optimized_tables() {
    let Some(params) = dba_params() else {
        return;
    };
    let mut dba = Harness::with(params);
    dba.session.send(DbRequest::SetAutocommit(true));
    let mut h = own_database(
        &mut dba,
        "r63_mem",
        &[
            "alter database [{db}] add filegroup [{db}_mo] contains memory_optimized_data",
            "declare @path nvarchar(400) = \
               cast(serverproperty('InstanceDefaultDataPath') as nvarchar(400)) + '{db}_mo'; \
             exec('alter database [{db}] add file (name = [{db}_mo], filename = ''' \
                  + @path + ''') to filegroup [{db}_mo]')",
        ],
    );
    for sql in [
        "create table r63_mo (\
           id int not null constraint r63_mo_pk primary key nonclustered hash \
             with (bucket_count = 1000), \
           a int not null, \
           b nvarchar(20) null, \
           c int identity(1, 1) not null, \
           n int not null constraint r63_mo_n default 0 constraint r63_mo_n_ck check (n >= 0), \
           constraint r63_mo_b unique nonclustered (b), \
           index r63_mo_a nonclustered (a desc, id), \
           index r63_mo_hash hash (a, id) with (bucket_count = 64)\
         ) with (memory_optimized = on)",
        "create table r63_mod (\
           id int not null constraint r63_mod_pk primary key nonclustered, \
           mo int null constraint r63_mod_fk references r63_mo (id), \
           v varchar(10) null, \
           index r63_mod_cs clustered columnstore\
         ) with (memory_optimized = on)",
        "create table r63_mos (id int not null primary key nonclustered hash \
         with (bucket_count = 8)) with (memory_optimized = on, durability = schema_only)",
    ] {
        h.ok(sql);
    }
    let ddl = |h: &mut Harness, table: &str| {
        let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
            owner: "dbo".into(),
            name: table.into(),
            object_type: ObjectType::Table,
        }) else {
            panic!("expected DDL")
        };
        ddl
    };
    let mo = ddl(&mut h, "r63_mo");
    assert_eq!(
        mo,
        "create table [dbo].[r63_mo] (\n    \
           [id] int not null,\n    \
           [a] int not null,\n    \
           [b] nvarchar(20) null,\n    \
           [c] int identity(1, 1) not null,\n    \
           [n] int not null constraint [r63_mo_n] default ((0)),\n    \
           constraint [r63_mo_pk] primary key nonclustered hash ([id]) \
             with (bucket_count = 1024),\n    \
           constraint [r63_mo_b] unique nonclustered ([b]),\n    \
           constraint [r63_mo_n_ck] check ([n]>=(0)),\n    \
           index [r63_mo_a] nonclustered ([a] desc, [id]),\n    \
           index [r63_mo_hash] nonclustered hash ([a], [id]) with (bucket_count = 64)\n\
         ) with (memory_optimized = on, durability = schema_and_data);\nGO\n"
    );
    let mo_d = ddl(&mut h, "r63_mod");
    assert!(
        mo_d.contains(
            "    constraint [r63_mod_fk] foreign key ([mo]) references [dbo].[r63_mo] ([id]),\n    \
             index [r63_mod_cs] clustered columnstore\n\
             ) with (memory_optimized = on, durability = schema_and_data);"
        ),
        "{mo_d}"
    );
    let mo_s = ddl(&mut h, "r63_mos");
    assert!(
        mo_s.ends_with(") with (memory_optimized = on, durability = schema_only);\nGO\n"),
        "{mo_s}"
    );
    // Run in another schema, the DDL makes the same tables.
    let tables = ["r63_mo", "r63_mod", "r63_mos"];
    let before = catalog_indexes(&mut h, "dbo", &tables);
    h.ok("create schema r63_copy");
    for ddl in [&mo, &mo_d, &mo_s] {
        run_ddl_in(&mut h, ddl, "dbo", "r63_copy", &tables);
    }
    assert_eq!(catalog_indexes(&mut h, "r63_copy", &tables), before);
    drop(h);
    drop_database(&mut dba, "r63_mem");
}

/// A table's DDL creates its selective XML indexes, with their namespaces
/// and their paths' types, and the secondary ones on a path of theirs
/// (#311): run elsewhere, the copy has the same indexes.
#[test]
fn selective_xml_indexes() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    h.ok("drop table if exists r63_copy.r63_sx");
    h.ok("drop schema if exists r63_copy");
    h.ok("drop table if exists r63_sx");
    for sql in [
        "create table r63_sx (id int constraint r63_sx_pk primary key, doc xml)",
        "create selective xml index [r63 sel] on r63_sx (doc) \
         with xmlnamespaces ('urn:a' as a, default 'urn:it''s') \
         for ([p 1] = '/a:x/a:y', p2 = '/a:x/a:z' as xquery 'xs:string' maxlength(20) singleton, \
              p3 = '/a:x/@n' as sql int, p4 = '/a:x/w' as xquery 'node()', \
              p5 = '/a:x/v' as sql nvarchar(30) singleton, p6 = '/a:x/u' as xquery 'xs:double', \
              p7 = '/a:x/t' as sql decimal(9,2), p8 = '/a:x/s' as sql varchar(max), \
              p9 = '/x' as xquery 'xs:string')",
        "create xml index r63_sx_n on r63_sx (doc) using xml index [r63 sel] for (p3)",
        "create xml index r63_sx_v on r63_sx (doc) using xml index [r63 sel] for (p5)",
    ] {
        h.ok(sql);
    }
    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: schema.clone(),
        name: "r63_sx".into(),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    let on = format!("on [{schema}].[r63_sx] ([doc])");
    for line in [
        format!(
            "create selective xml index [r63 sel] {on} \
             with xmlnamespaces ('urn:a' as [a], default 'urn:it''s') \
             for ([p 1] = '/a:x/a:y', [p2] = '/a:x/a:z' as xquery 'xs:string' maxlength(20) \
             singleton, [p3] = '/a:x/@n' as sql int, [p4] = '/a:x/w' as xquery 'node()', \
             [p5] = '/a:x/v' as sql nvarchar(30) singleton, [p6] = '/a:x/u' as xquery 'xs:double', \
             [p7] = '/a:x/t' as sql decimal(9,2), [p8] = '/a:x/s' as sql varchar(max), \
             [p9] = '/x' as xquery 'xs:string');"
        ),
        format!("create xml index [r63_sx_n] {on} using xml index [r63 sel] for ([p3]);"),
        format!("create xml index [r63_sx_v] {on} using xml index [r63 sel] for ([p5]);"),
    ] {
        assert!(ddl.contains(&line), "{line}\n{ddl}");
    }
    let selective = ddl.find("create selective").expect("the selective index");
    assert!(
        ddl.find("[r63_sx_n]").is_some_and(|at| at > selective),
        "{ddl}"
    );
    // The indexes, paths and namespaces as the catalog has them.
    let catalog = |h: &mut Harness, owner: &str| -> Vec<Option<String>> {
        let table = format!("object_id('[{owner}].[r63_sx]')");
        [
            format!(
                "select string_agg(concat_ws(' ', i.name, x.xml_index_type, u.name, p.name) \
                                   collate database_default, '; ') within group (order by i.name) \
                   from sys.xml_indexes x join sys.indexes i on i.object_id = x.object_id \
                    and i.index_id = x.index_id \
                   left join sys.indexes u on u.object_id = x.object_id \
                    and u.index_id = x.using_xml_index_id \
                   left join sys.selective_xml_index_paths p on p.object_id = x.object_id \
                    and p.index_id = x.using_xml_index_id and p.path_id = x.path_id \
                  where x.object_id = {table}"
            ),
            format!(
                "select string_agg(concat_ws(' ', p.path_id, p.name, p.path, p.path_type, \
                                             p.xquery_type_description, p.xquery_max_length, \
                                             p.is_node, type_name(p.user_type_id), p.max_length, \
                                             p.precision, p.scale, p.is_singleton) \
                                   collate database_default, '; ') \
                          within group (order by p.path_id) \
                   from sys.selective_xml_index_paths p where p.object_id = {table}"
            ),
            format!(
                "select string_agg(concat_ws(' ', n.is_default_uri, n.uri, n.prefix) \
                                   collate database_default, '; ') \
                          within group (order by n.is_default_uri) \
                   from sys.selective_xml_index_namespaces n where n.object_id = {table}"
            ),
        ]
        .iter()
        .map(|sql| h.scalar(sql))
        .collect()
    };
    let before = catalog(&mut h, &schema);
    assert!(before.iter().all(Option::is_some), "{before:?}");
    h.ok("create schema r63_copy");
    run_ddl_in(&mut h, &ddl, &schema, "r63_copy", &["r63_sx"]);
    assert_eq!(catalog(&mut h, "r63_copy"), before);
    h.ok("drop table r63_copy.r63_sx");
    h.ok("drop schema r63_copy");
    h.ok("drop table r63_sx");
}

/// The indexes of the tables in `owner` whose names start with `prefix`,
/// with their options and storage, and how each partition is compressed,
/// as the catalog has them.
fn catalog_storage(h: &mut Harness, owner: &str, prefix: &str, xml: bool) -> Vec<String> {
    let tables = format!(
        "select object_id from sys.tables where schema_id = schema_id('{owner}') \
            and name like '{prefix}%'"
    );
    let xml_compression = if xml { "p.xml_compression_desc" } else { "''" };
    [
        format!(
            "select string_agg(concat_ws(' ', t.name, l.name), '; ') within group (order by t.name) \
               from sys.tables t left join sys.data_spaces l on l.data_space_id = t.lob_data_space_id \
              where t.object_id in ({tables})"
        ),
        format!(
            "select string_agg(x.line collate database_default, '; ') within group (order by x.line) \
               from (select concat_ws(' ', object_name(i.object_id), i.name, \
                                      i.type_desc collate database_default, i.fill_factor, \
                                      i.is_padded, i.ignore_dup_key, i.allow_row_locks, \
                                      i.allow_page_locks, i.optimize_for_sequential_key, \
                                      i.compression_delay, s.no_recompute, s.is_incremental, \
                                      d.name, c.columns) as line \
                       from sys.indexes i \
                       left join sys.stats s on s.object_id = i.object_id and s.stats_id = i.index_id \
                       left join sys.data_spaces d on d.data_space_id = i.data_space_id \
                      outer apply (select string_agg(col_name(ic.object_id, ic.column_id) \
                                                     + ' ' + cast(ic.key_ordinal as varchar(3)) \
                                                     + ' ' + cast(ic.partition_ordinal as varchar(3)) \
                                                     + ' ' + cast(ic.is_included_column as varchar(3)), \
                                                     ',') \
                                            within group (order by ic.index_column_id) as columns \
                                     from sys.index_columns ic \
                                    where ic.object_id = i.object_id \
                                      and ic.index_id = i.index_id) c \
                      where i.object_id in ({tables})) x"
        ),
        format!(
            "select string_agg(concat_ws(' ', object_name(p.object_id), i.name, p.partition_number, \
                                         p.data_compression_desc, {xml_compression}) \
                                 collate database_default, '; ') \
                      within group (order by object_name(p.object_id), i.name, p.partition_number) \
               from sys.partitions p join sys.indexes i on i.object_id = p.object_id \
                and i.index_id = p.index_id \
              where p.object_id in ({tables})"
        ),
    ]
    .iter()
    .filter_map(|sql| h.scalar(sql))
    .collect()
}

/// A table's DDL has its indexes' options where they aren't the defaults,
/// their compression, also by partition, and the filegroup or partition
/// scheme of the table, its indexes and its LOB data (#315): run in another
/// schema, it makes the same indexes, stored alike. An index on a partition
/// scheme has the partitioning column, which isn't one of its columns.
#[test]
fn index_options_and_storage() {
    let Some(params) = dba_params() else {
        return;
    };
    let mut dba = Harness::with(params);
    dba.session.send(DbRequest::SetAutocommit(true));
    let mut h = own_database(
        &mut dba,
        "r63_opts",
        &[
            "alter database [{db}] add filegroup [r63_fg]",
            "declare @path nvarchar(400) = \
               cast(serverproperty('InstanceDefaultDataPath') as nvarchar(400)) + '{db}_fg.ndf'; \
             exec('alter database [{db}] add file (name = [{db}_fg], filename = ''' \
                  + @path + ''') to filegroup [r63_fg]')",
        ],
    );
    let major = h
        .scalar("select cast(serverproperty('ProductMajorVersion') as int)")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_default();
    // OPTIMIZE_FOR_SEQUENTIAL_KEY came with SQL Server 2019, XML_COMPRESSION
    // with 2022.
    let (sequential, xml) = (major >= 15, major >= 16);
    let sequential_key = if sequential {
        ", optimize_for_sequential_key = on"
    } else {
        ""
    };
    h.ok(
        "create table r63_o (id int not null, a int, b varchar(10), d varchar(max), \
           constraint r63_o_pk primary key clustered (id) \
             with (fillfactor = 80, pad_index = on, data_compression = page) on [r63_fg], \
           constraint r63_o_a unique (a) \
             with (ignore_dup_key = on, allow_row_locks = off, statistics_norecompute = on) \
             on [primary]) \
         on [r63_fg] textimage_on [primary]",
    );
    h.ok(&format!(
        "create index r63_o_b on r63_o (b) \
         with (allow_page_locks = off{sequential_key}, data_compression = row) on [r63_fg]"
    ));
    for sql in [
        "create partition function r63_pf (int) as range right for values (10, 20)",
        "create partition scheme r63_ps as partition r63_pf to ([primary], [r63_fg], [primary])",
        "create index r63_o_plain on r63_o (b, a)",
        "create table r63_h (id int, v int) on [r63_fg] with (data_compression = row)",
        "create table r63_p (id int not null, v int, w int) on r63_ps (id)",
        "create index r63_p_v on r63_p (v) include (w) \
         with (data_compression = page on partitions (1, 3), \
               data_compression = row on partitions (2), statistics_incremental = on)",
        "create unique index r63_p_id on r63_p (id desc) on [primary]",
        "create table r63_c (id int not null, v int) on r63_ps (id)",
        "create clustered columnstore index r63_c_cs on r63_c \
         with (data_compression = columnstore_archive on partitions (2, 3), \
               compression_delay = 5) \
         on r63_ps (id)",
        "create table r63_g (id int constraint r63_g_pk primary key, shape geometry)",
        "create spatial index r63_g_s on r63_g (shape) using geometry_auto_grid \
         with (bounding_box = (0, 0, 10, 10), fillfactor = 60, pad_index = on) on [r63_fg]",
    ] {
        h.ok(sql);
    }
    if xml {
        h.ok("create table r63_x (id int, doc xml) with (xml_compression = on)");
    }
    let ddl = |h: &mut Harness, table: &str| {
        let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
            owner: "dbo".into(),
            name: table.into(),
            object_type: ObjectType::Table,
        }) else {
            panic!("expected DDL")
        };
        ddl
    };
    let mut tables = vec!["r63_o", "r63_h", "r63_p", "r63_c", "r63_g"];
    if xml {
        tables.push("r63_x");
    }
    let ddls: Vec<String> = tables.iter().map(|t| ddl(&mut h, t)).collect();
    let mut lines = vec![
        "    constraint [r63_o_pk] primary key clustered ([id]) \
         with (pad_index = on, fillfactor = 80, data_compression = page),"
            .to_owned(),
        "    constraint [r63_o_a] unique nonclustered ([a]) \
         with (ignore_dup_key = on, statistics_norecompute = on, allow_row_locks = off) \
         on [PRIMARY]\n) on [r63_fg] textimage_on [PRIMARY];"
            .to_owned(),
        format!(
            "create nonclustered index [r63_o_b] on [dbo].[r63_o] ([b]) \
             with (allow_page_locks = off{sequential_key}, data_compression = row);"
        ),
        "create nonclustered index [r63_o_plain] on [dbo].[r63_o] ([b], [a]);".to_owned(),
        ") on [r63_fg] with (data_compression = row);".to_owned(),
        ") on [r63_ps] ([id]);".to_owned(),
        "create unique nonclustered index [r63_p_id] on [dbo].[r63_p] ([id] desc) on [PRIMARY];"
            .to_owned(),
        "create nonclustered index [r63_p_v] on [dbo].[r63_p] ([v]) include ([w]) \
         with (statistics_incremental = on, data_compression = page on partitions (1, 3), \
         data_compression = row on partitions (2));"
            .to_owned(),
        "create clustered columnstore index [r63_c_cs] on [dbo].[r63_c] \
         with (compression_delay = 5, data_compression = columnstore_archive on partitions (2 to 3));"
            .to_owned(),
        "create spatial index [r63_g_s] on [dbo].[r63_g] ([shape]) using geometry_auto_grid \
         with (bounding_box = (0, 0, 10, 10), cells_per_object = 8, pad_index = on, \
         fillfactor = 60) on [r63_fg];"
            .to_owned(),
    ];
    if xml {
        lines.push(") with (xml_compression = on);".to_owned());
    }
    let all = ddls.join("\n");
    for line in lines {
        assert!(all.contains(&line), "{line}\n{all}");
    }
    // Indexes leaves out the partitioning column an index on a partition
    // scheme has too.
    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: "dbo".into(),
        name: "r63_p".into(),
        detail: Detail::Indexes,
    }) else {
        panic!("expected a table")
    };
    let p_v = rows
        .iter()
        .find(|r| r[0].as_deref() == Some("r63_p_v"))
        .expect("r63_p_v");
    assert_eq!(
        (p_v[3].as_deref(), p_v[4].as_deref()),
        (Some("v"), Some("w"))
    );
    let before = catalog_storage(&mut h, "dbo", "r63_", xml);
    h.ok("create schema r63_copy");
    for ddl in &ddls {
        run_ddl_in(&mut h, ddl, "dbo", "r63_copy", &tables);
    }
    assert_eq!(catalog_storage(&mut h, "r63_copy", "r63_", xml), before);
    drop(h);
    drop_database(&mut dba, "r63_opts");
}
/// A table's DDL, as `MetaQuery::Ddl` reads it.
fn table_ddl(h: &mut Harness, owner: &str, table: &str) -> String {
    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: owner.into(),
        name: table.into(),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    ddl
}

/// Drops the tables of `schema` whose names start with `prefix`, with their
/// history tables and foreign keys; a temporal table's versioning goes off
/// first.
fn drop_tables(h: &mut Harness, schema: &str, prefix: &str) {
    h.ok(&format!(
        "declare @sql nvarchar(max) = N''; \
         select @sql += N'alter table ' + quotename(schema_name(t.schema_id)) + N'.' \
                        + quotename(t.name) + N' set (system_versioning = off); ' \
           from sys.tables t \
          where t.schema_id = schema_id('{schema}') and t.name like '{prefix}%' \
            and t.temporal_type = 2; \
         select @sql += N'alter table ' + quotename(schema_name(t.schema_id)) + N'.' \
                        + quotename(t.name) + N' drop constraint ' + quotename(f.name) + N'; ' \
           from sys.foreign_keys f join sys.tables t on t.object_id = f.parent_object_id \
          where t.schema_id = schema_id('{schema}') and t.name like '{prefix}%'; \
         select @sql += N'drop table ' + quotename(schema_name(t.schema_id)) + N'.' \
                        + quotename(t.name) + N'; ' \
           from sys.tables t \
          where t.schema_id = schema_id('{schema}') and t.name like '{prefix}%' \
             or t.object_id in (select h.history_table_id from sys.tables h \
                                 where h.schema_id = schema_id('{schema}') \
                                   and h.name like '{prefix}%'); \
         exec (@sql)"
    ));
}

/// A table's DDL collates its columns that aren't in the database's
/// collation (#323): run elsewhere, a case-sensitive column stays one, and
/// one in the database's collation takes that database's.
#[test]
fn column_collations() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    drop_tables(&mut h, "r74_copy_co", "r74_co");
    h.ok("drop schema if exists r74_copy_co");
    drop_tables(&mut h, &schema, "r74_co");
    let default = h
        .scalar("select cast(databasepropertyex(db_name(), 'Collation') as nvarchar(128))")
        .expect("the database's collation");
    h.ok(&format!(
        "create table r74_co (id int, cs varchar(10) collate Latin1_General_CS_AS not null, \
         d varchar(10), dd nvarchar(10) collate {default}, \
         u varchar(max) collate Latin1_General_100_CI_AS_SC_UTF8, \
         b char(2) collate Latin1_General_BIN2 constraint r74_co_b default 'x', \
         up as upper(cs), t text collate Latin1_General_CS_AS)"
    ));
    let ddl = table_ddl(&mut h, &schema, "r74_co");
    assert_eq!(
        ddl,
        format!(
            "create table [{schema}].[r74_co] (\n    \
               [id] int null,\n    \
               [cs] varchar(10) collate Latin1_General_CS_AS not null,\n    \
               [d] varchar(10) null,\n    \
               [dd] nvarchar(10) null,\n    \
               [u] varchar(max) collate Latin1_General_100_CI_AS_SC_UTF8 null,\n    \
               [b] char(2) collate Latin1_General_BIN2 null constraint [r74_co_b] default ('x'),\n    \
               [up] as (upper([cs])),\n    \
               [t] text collate Latin1_General_CS_AS null\n\
             );\nGO\n"
        )
    );
    let catalog = |h: &mut Harness, owner: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', c.name, c.collation_name), '; ') \
                      within group (order by c.column_id) \
               from sys.columns c where c.object_id = object_id('[{owner}].[r74_co]')"
        ))
    };
    let before = catalog(&mut h, &schema);
    h.ok("create schema r74_copy_co");
    run_ddl_in(&mut h, &ddl, &schema, "r74_copy_co", &["r74_co"]);
    assert_eq!(catalog(&mut h, "r74_copy_co"), before);
    // The copy compares and keeps text as the original does.
    for owner in [schema.as_str(), "r74_copy_co"] {
        h.ok(&format!(
            "insert into [{owner}].[r74_co] (id, cs, u, t) values (1, 'a', N'čaj 茶', 'a')"
        ));
        assert_eq!(
            h.scalar(&format!(
                "select concat((select count(*) from [{owner}].[r74_co] \
                                 where cs = 'A' or t like 'A'), \
                               ' ', (select u from [{owner}].[r74_co]))"
            ))
            .as_deref(),
            Some("0 čaj 茶"),
            "{owner}"
        );
    }
    drop_tables(&mut h, "r74_copy_co", "r74_co");
    h.ok("drop schema r74_copy_co");
    drop_tables(&mut h, &schema, "r74_co");
}

/// The temporal tables of `owner` whose names start with `prefix`, as the
/// catalog has them: their versioning, history table and retention, their
/// periods, generated and hidden columns, and their history tables'
/// columns and indexes. A history table SQL Server named, and its index,
/// read with `N` for the table's object_id.
fn catalog_temporal(h: &mut Harness, owner: &str, prefix: &str, retention: bool) -> Vec<String> {
    let tables = format!(
        "select object_id from sys.tables where schema_id = schema_id('{owner}') \
            and name like '{prefix}%' and temporal_type <> 1"
    );
    let retention = if retention {
        "t.history_retention_period, t.history_retention_period_unit_desc"
    } else {
        "''"
    };
    [
        format!(
            "select string_agg(concat_ws(' ', t.name, t.temporal_type, \
                                         replace(object_name(t.history_table_id), \
                                                 cast(t.object_id as varchar(20)), 'N'), \
                                         case when object_schema_name(t.history_table_id) \
                                                   = schema_name(t.schema_id) then 'own' end, \
                                         {retention}, \
                                         p.name, col_name(p.object_id, p.start_column_id), \
                                         col_name(p.object_id, p.end_column_id)) \
                               collate database_default, '; ') within group (order by t.name) \
               from sys.tables t left join sys.periods p on p.object_id = t.object_id \
              where t.object_id in ({tables})"
        ),
        format!(
            "select string_agg(concat_ws(' ', object_name(c.object_id), c.name, \
                                         type_name(c.user_type_id), c.scale, c.is_nullable, \
                                         c.generated_always_type, c.is_hidden) \
                               collate database_default, '; ') \
                      within group (order by object_name(c.object_id), c.column_id) \
               from sys.columns c where c.object_id in ({tables})"
        ),
        format!(
            "select string_agg(concat_ws(' ', t.name, c.name, type_name(c.user_type_id), c.scale, \
                                         c.is_nullable, c.generated_always_type, c.is_hidden) \
                               collate database_default, '; ') \
                      within group (order by t.name, c.column_id) \
               from sys.tables t join sys.columns c on c.object_id = t.history_table_id \
              where t.object_id in ({tables})"
        ),
        format!(
            "select string_agg(concat_ws(' ', t.name, \
                                         replace(i.name, cast(t.object_id as varchar(20)), 'N'), \
                                         i.type_desc, p.data_compression_desc, cols.names) \
                               collate database_default, '; ') within group (order by t.name, i.name) \
               from sys.tables t join sys.indexes i on i.object_id = t.history_table_id \
               join sys.partitions p on p.object_id = i.object_id and p.index_id = i.index_id \
              outer apply (select string_agg(col_name(ic.object_id, ic.column_id), ',') \
                                    within group (order by ic.key_ordinal) as names \
                             from sys.index_columns ic \
                            where ic.object_id = i.object_id and ic.index_id = i.index_id) cols \
              where t.object_id in ({tables})"
        ),
    ]
    .iter()
    .filter_map(|sql| h.scalar(sql))
    .collect()
}

/// A temporal table's DDL makes one (#324): its period for system_time, its
/// generated always columns, hidden ones too, and its system versioning
/// with its history table and retention. Run elsewhere, SQL Server makes
/// the history table as it made the original's, or uses one the history
/// table's DDL made before; a history table SQL Server named is named for
/// the copy.
#[test]
fn temporal_tables() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    drop_tables(&mut h, "r74_copy_t", "r74_t");
    h.ok("drop schema if exists r74_copy_t");
    drop_tables(&mut h, &schema, "r74_t");
    let major = h
        .scalar("select cast(serverproperty('ProductMajorVersion') as int)")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_default();
    // HISTORY_RETENTION_PERIOD came with SQL Server 2017.
    let retention = major >= 14;
    let keep = if retention {
        ", history_retention_period = 6 months"
    } else {
        ""
    };
    for sql in [
        "create table r74_tp (id int constraint r74_tp_pk primary key, v varchar(10), \
           f datetime2 generated always as row start hidden not null, \
           u datetime2 generated always as row end hidden not null, \
           period for system_time (f, u)) \
         with (system_versioning = on)"
            .to_owned(),
        format!(
            "create table r74_tq (id int constraint r74_tq_pk primary key, \
               f datetime2(3) generated always as row start not null \
                 constraint r74_tq_df default sysutcdatetime(), \
               u datetime2(3) generated always as row end not null \
                 constraint r74_tq_du default '9999-12-31 23:59:59.999', \
               period for system_time (f, u)) \
             with (system_versioning = on (history_table = [{schema}].[r74_tq_hist]{keep}))"
        ),
        "create table r74_tr (id int, f datetime2 generated always as row start not null, \
           u datetime2 generated always as row end not null, period for system_time (f, u))"
            .to_owned(),
    ] {
        h.ok(&sql);
    }
    let tp = table_ddl(&mut h, &schema, "r74_tp");
    assert_eq!(
        tp,
        format!(
            "create table [{schema}].[r74_tp] (\n    \
               [id] int not null,\n    \
               [v] varchar(10) null,\n    \
               [f] datetime2(7) generated always as row start hidden not null,\n    \
               [u] datetime2(7) generated always as row end hidden not null,\n    \
               constraint [r74_tp_pk] primary key clustered ([id]),\n    \
               period for system_time ([f], [u])\n\
             ) with (system_versioning = on);\nGO\n"
        )
    );
    let tq = table_ddl(&mut h, &schema, "r74_tq");
    assert_eq!(
        tq,
        format!(
            "create table [{schema}].[r74_tq] (\n    \
               [id] int not null,\n    \
               [f] datetime2(3) generated always as row start not null \
                 constraint [r74_tq_df] default (sysutcdatetime()),\n    \
               [u] datetime2(3) generated always as row end not null \
                 constraint [r74_tq_du] default ('9999-12-31 23:59:59.999'),\n    \
               constraint [r74_tq_pk] primary key clustered ([id]),\n    \
               period for system_time ([f], [u])\n\
             ) with (system_versioning = on (history_table = [{schema}].[r74_tq_hist]{keep}));\
             \nGO\n"
        )
    );
    let tr = table_ddl(&mut h, &schema, "r74_tr");
    assert_eq!(
        tr,
        format!(
            "create table [{schema}].[r74_tr] (\n    \
               [id] int null,\n    \
               [f] datetime2(7) generated always as row start not null,\n    \
               [u] datetime2(7) generated always as row end not null,\n    \
               period for system_time ([f], [u])\n\
             );\nGO\n"
        )
    );
    // The history table's own DDL makes a table SQL Server takes for one.
    let hist = table_ddl(&mut h, &schema, "r74_tq_hist");
    assert!(
        hist.starts_with(&format!(
            "create table [{schema}].[r74_tq_hist] (\n    \
               [id] int not null,\n    \
               [f] datetime2(3) not null,\n    \
               [u] datetime2(3) not null\n\
             );"
        )),
        "{hist}"
    );
    let before = catalog_temporal(&mut h, &schema, "r74_t", retention);
    assert_eq!(before.len(), 4, "{before:?}");
    h.ok("create schema r74_copy_t");
    let tables = ["r74_tp", "r74_tq", "r74_tq_hist", "r74_tr"];
    for ddl in [&tp, &hist, &tq, &tr] {
        run_ddl_in(&mut h, ddl, &schema, "r74_copy_t", &tables);
    }
    assert_eq!(
        catalog_temporal(&mut h, "r74_copy_t", "r74_t", retention),
        before
    );
    // The copy keeps the rows' versions.
    h.ok("insert into r74_copy_t.r74_tp (id, v) values (1, 'a')");
    h.ok("update r74_copy_t.r74_tp set v = 'b'");
    assert_eq!(
        h.scalar("select count(*) from r74_copy_t.r74_tp for system_time all")
            .as_deref(),
        Some("2")
    );
    drop_tables(&mut h, "r74_copy_t", "r74_t");
    h.ok("drop schema r74_copy_t");
    drop_tables(&mut h, &schema, "r74_t");
}

/// A table's DDL keeps its sparse columns and its column set (#325): run
/// elsewhere, the columns are sparse again and `select *` reads them
/// through the column set.
#[test]
fn sparse_columns_and_column_sets() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    drop_tables(&mut h, "r74_copy_s", "r74_s");
    h.ok("drop schema if exists r74_copy_s");
    drop_tables(&mut h, &schema, "r74_s");
    for sql in [
        "create table r74_sp (id int not null, a int sparse null, \
         b varchar(10) collate Latin1_General_CS_AS sparse, \
         cs xml column_set for all_sparse_columns)",
        "create table r74_sq (id int, a datetime2(3) sparse null)",
    ] {
        h.ok(sql);
    }
    let sp = table_ddl(&mut h, &schema, "r74_sp");
    assert_eq!(
        sp,
        format!(
            "create table [{schema}].[r74_sp] (\n    \
               [id] int not null,\n    \
               [a] int sparse null,\n    \
               [b] varchar(10) collate Latin1_General_CS_AS sparse null,\n    \
               [cs] xml column_set for all_sparse_columns null\n\
             );\nGO\n"
        )
    );
    let sq = table_ddl(&mut h, &schema, "r74_sq");
    assert!(sq.contains("    [a] datetime2(3) sparse null\n"), "{sq}");
    let catalog = |h: &mut Harness, owner: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', object_name(c.object_id), c.name, c.is_sparse, \
                                         c.is_column_set, c.is_nullable, c.collation_name) \
                               collate database_default, '; ') \
                      within group (order by object_name(c.object_id), c.column_id) \
               from sys.columns c \
              where c.object_id in (object_id('[{owner}].[r74_sp]'), object_id('[{owner}].[r74_sq]'))"
        ))
    };
    let before = catalog(&mut h, &schema);
    h.ok("create schema r74_copy_s");
    for ddl in [&sp, &sq] {
        run_ddl_in(&mut h, ddl, &schema, "r74_copy_s", &["r74_sp", "r74_sq"]);
    }
    assert_eq!(catalog(&mut h, "r74_copy_s"), before);
    // Rows go in and come out through the column set.
    h.ok("insert into r74_copy_s.r74_sp (id, cs) values (1, '<a>5</a>')");
    assert_eq!(
        h.scalar("select concat(a, ' ', cast(cs as nvarchar(100))) from r74_copy_s.r74_sp")
            .as_deref(),
        Some("5 <a>5</a>")
    );
    drop_tables(&mut h, "r74_copy_s", "r74_s");
    h.ok("drop schema r74_copy_s");
    drop_tables(&mut h, &schema, "r74_s");
}

/// A table's DDL keeps the states CREATE TABLE can't give (#326): foreign
/// keys and checks added WITH NOCHECK, or disabled, are added so after the
/// table, disabled indexes are disabled, and its lock escalation is set.
/// Run elsewhere, the copy's constraints and indexes are as trusted and
/// enabled as the original's.
#[test]
fn constraint_and_index_states() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    drop_tables(&mut h, "r74_copy_k", "r74_k");
    h.ok("drop schema if exists r74_copy_k");
    drop_tables(&mut h, &schema, "r74_k");
    for sql in [
        "create table r74_kp (id int constraint r74_kp_pk primary key, a int, \
         constraint r74_kp_u unique (a))",
        "create table r74_kc (id int, k int, n int, \
         constraint r74_kc_fk foreign key (k) references r74_kp (id), \
         constraint r74_kc_ck check (n > 0))",
        "alter table r74_kc add constraint r74_kc_fk_off foreign key (id) references r74_kp (id)",
        "alter table r74_kc add constraint r74_kc_ck_off check (k <> 0)",
        "alter table r74_kc nocheck constraint r74_kc_fk_off",
        "alter table r74_kc nocheck constraint r74_kc_ck_off",
        "insert into r74_kc values (7, null, 500)",
        "alter table r74_kc with nocheck add constraint r74_kc_fk_nc foreign key (n) \
         references r74_kp (id) on delete cascade",
        "alter table r74_kc with nocheck add constraint r74_kc_ck_nc check (n < 100)",
        "create index r74_kc_k on r74_kc (k)",
        "create index r74_kc_n on r74_kc (n)",
        "alter index r74_kc_n on r74_kc disable",
        "alter index r74_kp_u on r74_kp disable",
        "alter table r74_kc set (lock_escalation = auto)",
        "alter table r74_kp set (lock_escalation = disable)",
        "create table r74_km (id int, v int)",
        "create clustered index r74_km_c on r74_km (id)",
        "create index r74_km_v on r74_km (v)",
        "alter index r74_km_c on r74_km disable",
    ] {
        h.ok(sql);
    }
    let kp = table_ddl(&mut h, &schema, "r74_kp");
    let t = format!("[{schema}].[r74_kp]");
    assert_eq!(
        kp,
        format!(
            "create table {t} (\n    \
               [id] int not null,\n    \
               [a] int null,\n    \
               constraint [r74_kp_pk] primary key clustered ([id]),\n    \
               constraint [r74_kp_u] unique nonclustered ([a])\n\
             );\n\n\
             alter table {t} set (lock_escalation = disable);\n\n\
             alter index [r74_kp_u] on {t} disable;\nGO\n"
        )
    );
    let kc = table_ddl(&mut h, &schema, "r74_kc");
    let t = format!("[{schema}].[r74_kc]");
    let p = format!("[{schema}].[r74_kp]");
    assert_eq!(
        kc,
        format!(
            "create table {t} (\n    \
               [id] int null,\n    \
               [k] int null,\n    \
               [n] int null,\n    \
               constraint [r74_kc_fk] foreign key ([k]) references {p} ([id]),\n    \
               constraint [r74_kc_ck] check ([n]>(0))\n\
             );\n\n\
             alter table {t} set (lock_escalation = auto);\n\n\
             create nonclustered index [r74_kc_k] on {t} ([k]);\n\n\
             create nonclustered index [r74_kc_n] on {t} ([n]);\n\n\
             alter table {t} with nocheck add constraint [r74_kc_fk_nc] foreign key ([n]) \
               references {p} ([id]) on delete cascade;\n\n\
             alter table {t} with nocheck add constraint [r74_kc_fk_off] foreign key ([id]) \
               references {p} ([id]);\n\n\
             alter table {t} nocheck constraint [r74_kc_fk_off];\n\n\
             alter table {t} with nocheck add constraint [r74_kc_ck_nc] check ([n]<(100));\n\n\
             alter table {t} with nocheck add constraint [r74_kc_ck_off] check ([k]<>(0));\n\n\
             alter table {t} nocheck constraint [r74_kc_ck_off];\n\n\
             alter index [r74_kc_n] on {t} disable;\nGO\n"
        )
    );
    // A disabled clustered index disables the others too: they are
    // disabled first, while the table can be read.
    let km = table_ddl(&mut h, &schema, "r74_km");
    let t = format!("[{schema}].[r74_km]");
    assert!(
        km.ends_with(&format!(
            "create clustered index [r74_km_c] on {t} ([id]);\n\n\
             create nonclustered index [r74_km_v] on {t} ([v]);\n\n\
             alter index [r74_km_v] on {t} disable;\n\n\
             alter index [r74_km_c] on {t} disable;\nGO\n"
        )),
        "{km}"
    );
    let catalog = |h: &mut Harness, owner: &str| -> Vec<Option<String>> {
        let tables = format!(
            "select object_id from sys.tables where schema_id = schema_id('{owner}') \
                and name like 'r74_k%'"
        );
        [
            format!(
                "select string_agg(concat_ws(' ', t.name, t.lock_escalation_desc) \
                                   collate database_default, '; ') within group (order by t.name) \
                   from sys.tables t where t.object_id in ({tables})"
            ),
            format!(
                "select string_agg(x.line collate database_default, '; ') \
                          within group (order by x.line) \
                   from (select concat_ws(' ', object_name(parent_object_id), name, is_disabled, \
                                          is_not_trusted, delete_referential_action) as line \
                           from sys.foreign_keys where parent_object_id in ({tables}) \
                         union all \
                         select concat_ws(' ', object_name(parent_object_id), name, is_disabled, \
                                          is_not_trusted, definition) \
                           from sys.check_constraints where parent_object_id in ({tables})) x"
            ),
            format!(
                "select string_agg(concat_ws(' ', object_name(i.object_id), i.name, \
                                             i.type_desc, i.is_disabled) \
                                   collate database_default, '; ') \
                          within group (order by object_name(i.object_id), i.name) \
                   from sys.indexes i where i.object_id in ({tables}) and i.type > 0"
            ),
        ]
        .iter()
        .map(|sql| h.scalar(sql))
        .collect()
    };
    let before = catalog(&mut h, &schema);
    h.ok("create schema r74_copy_k");
    let tables = ["r74_kp", "r74_kc", "r74_km"];
    for ddl in [&kp, &kc, &km] {
        run_ddl_in(&mut h, ddl, &schema, "r74_copy_k", &tables);
    }
    assert_eq!(catalog(&mut h, "r74_copy_k"), before);
    // Rows the disabled constraints let into the original go into the copy.
    for owner in [schema.as_str(), "r74_copy_k"] {
        h.ok(&format!(
            "insert into [{owner}].[r74_kp] (id) values (0), (50)"
        ));
        h.ok(&format!(
            "insert into [{owner}].[r74_kc] values (99, 0, 50)"
        ));
    }
    drop_tables(&mut h, "r74_copy_k", "r74_k");
    h.ok("drop schema r74_copy_k");
    drop_tables(&mut h, &schema, "r74_k");
}

/// A table's DDL builds its clustered index first, so that the
/// nonclustered ones aren't built again over it, and leaves out
/// hypothetical indexes, which have statistics and no rows (#327): run
/// elsewhere, the copy has the same indexes, all of them real.
#[test]
fn clustered_first_without_hypothetical_indexes() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    drop_tables(&mut h, "r74_copy_h", "r74_h");
    h.ok("drop schema if exists r74_copy_h");
    drop_tables(&mut h, &schema, "r74_h");
    for sql in [
        "create table r74_hy (id int not null, a int, b int)",
        "create index r74_hy_a on r74_hy (a)",
        "create clustered index r74_hy_z on r74_hy (id)",
        "create index r74_hy_hypo on r74_hy (b) with statistics_only = -1",
        "create table r74_hc (id int, a int)",
        "create index r74_hc_a on r74_hc (a)",
        "create clustered columnstore index r74_hc_z on r74_hc",
        "create table r74_hd (id int, a int)",
        "create clustered index r74_hd_c on r74_hd (id)",
        "create index r74_hd_hypo on r74_hd (a) with statistics_only = -1",
        "alter index r74_hd_c on r74_hd disable",
    ] {
        h.ok(sql);
    }
    assert_eq!(
        h.scalar(
            "select count(*) from sys.indexes \
              where name in ('r74_hy_hypo', 'r74_hd_hypo') and is_hypothetical = 1"
        )
        .as_deref(),
        Some("2")
    );
    let hy = table_ddl(&mut h, &schema, "r74_hy");
    let t = format!("[{schema}].[r74_hy]");
    assert!(
        hy.ends_with(&format!(
            ");\n\n\
             create clustered index [r74_hy_z] on {t} ([id]);\n\n\
             create nonclustered index [r74_hy_a] on {t} ([a]);\nGO\n"
        )),
        "{hy}"
    );
    let hc = table_ddl(&mut h, &schema, "r74_hc");
    let t = format!("[{schema}].[r74_hc]");
    assert!(
        hc.ends_with(&format!(
            ");\n\n\
             create clustered columnstore index [r74_hc_z] on {t};\n\n\
             create nonclustered index [r74_hc_a] on {t} ([a]);\nGO\n"
        )),
        "{hc}"
    );
    // Disabling a clustered index disables the hypothetical ones too: they
    // aren't disabled in the DDL either.
    let hd = table_ddl(&mut h, &schema, "r74_hd");
    let t = format!("[{schema}].[r74_hd]");
    assert!(
        hd.ends_with(&format!(
            ");\n\n\
             create clustered index [r74_hd_c] on {t} ([id]);\n\n\
             alter index [r74_hd_c] on {t} disable;\nGO\n"
        )),
        "{hd}"
    );
    let catalog = |h: &mut Harness, owner: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', object_name(i.object_id), i.name, i.type_desc, \
                                         i.is_hypothetical, i.is_disabled) \
                               collate database_default, '; ') \
                      within group (order by object_name(i.object_id), i.name) \
               from sys.indexes i \
              where i.object_id in (select object_id from sys.tables \
                                     where schema_id = schema_id('{owner}') \
                                       and name like 'r74_h%') \
                and i.type > 0 and i.is_hypothetical = 0"
        ))
    };
    let before = catalog(&mut h, &schema);
    h.ok("create schema r74_copy_h");
    let tables = ["r74_hy", "r74_hc", "r74_hd"];
    for ddl in [&hy, &hc, &hd] {
        run_ddl_in(&mut h, ddl, &schema, "r74_copy_h", &tables);
    }
    assert_eq!(catalog(&mut h, "r74_copy_h"), before);
    assert_eq!(
        h.scalar(
            "select count(*) from sys.indexes \
              where object_schema_name(object_id) = 'r74_copy_h' and is_hypothetical = 1"
        )
        .as_deref(),
        Some("0")
    );
    drop_tables(&mut h, "r74_copy_h", "r74_h");
    h.ok("drop schema r74_copy_h");
    drop_tables(&mut h, &schema, "r74_h");
}

/// A table's DDL keeps its columns' dynamic data masks (#348): run
/// elsewhere, a user without UNMASK reads the copy's columns masked, as
/// the original's.
#[test]
fn masked_columns() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    drop_tables(&mut h, "r82_copy_m", "r82_m");
    h.ok("drop schema if exists r82_copy_m");
    drop_tables(&mut h, &schema, "r82_m");
    h.ok("drop user if exists r82_m_reader");
    h.ok(
        "create table r82_m (id int, e varchar(100) masked with (function = 'email()') not null, \
         p varchar(20) collate Latin1_General_CS_AS \
           masked with (function = 'partial(1, \"X''X\", 2)'), \
         d date masked with (function = 'default()') constraint r82_m_d default '2020-01-01', \
         n int masked with (function = 'random(5, 5)'))",
    );
    let ddl = table_ddl(&mut h, &schema, "r82_m");
    assert_eq!(
        ddl,
        format!(
            "create table [{schema}].[r82_m] (\n    \
               [id] int null,\n    \
               [e] varchar(100) masked with (function = 'email()') not null,\n    \
               [p] varchar(20) collate Latin1_General_CS_AS \
                 masked with (function = 'partial(1, \"X''X\", 2)') null,\n    \
               [d] date masked with (function = 'default()') null \
                 constraint [r82_m_d] default ('2020-01-01'),\n    \
               [n] int masked with (function = 'random(5, 5)') null\n\
             );\nGO\n"
        )
    );
    let catalog = |h: &mut Harness, owner: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', c.name, c.is_masked, m.masking_function) \
                               collate database_default, '; ') \
                      within group (order by c.column_id) \
               from sys.columns c \
               left join sys.masked_columns m on m.object_id = c.object_id \
                and m.column_id = c.column_id \
              where c.object_id = object_id('[{owner}].[r82_m]')"
        ))
    };
    let before = catalog(&mut h, &schema);
    h.ok("create schema r82_copy_m");
    run_ddl_in(&mut h, &ddl, &schema, "r82_copy_m", &["r82_m"]);
    assert_eq!(catalog(&mut h, "r82_copy_m"), before);
    // A user without UNMASK reads both masked.
    h.ok("create user r82_m_reader without login");
    for owner in [schema.as_str(), "r82_copy_m"] {
        h.ok(&format!(
            "insert into [{owner}].[r82_m] values (1, 'alice@example.com', '0791234567', \
             '2021-05-06', 7)"
        ));
        h.ok(&format!(
            "grant select on [{owner}].[r82_m] to r82_m_reader"
        ));
        h.ok("execute as user = 'r82_m_reader'");
        let read = h.first_row(&format!("select e, p, d, n from [{owner}].[r82_m]"));
        h.ok("revert");
        assert_eq!(
            read,
            ["aXXX@XXXX.com", "0X'X67", "1900-01-01", "5"],
            "{owner}"
        );
    }
    h.ok("drop user r82_m_reader");
    drop_tables(&mut h, "r82_copy_m", "r82_m");
    h.ok("drop schema r82_copy_m");
    drop_tables(&mut h, &schema, "r82_m");
}

/// A table's DDL types its XML columns with their schema collections, as
/// content or document (#349): run elsewhere, the copy's columns take only
/// the XML the collection allows, as the original's.
#[test]
fn typed_xml_columns() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    drop_tables(&mut h, "r82_copy_x", "r82_x");
    h.ok("drop schema if exists r82_copy_x");
    drop_tables(&mut h, &schema, "r82_x");
    h.ok(
        "if exists (select 1 from sys.xml_schema_collections where name = 'r82_xs') \
         drop xml schema collection r82_xs",
    );
    h.ok("create xml schema collection r82_xs as \
         N'<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\
           <xs:element name=\"a\" type=\"xs:int\"/></xs:schema>'");
    h.ok(
        "create table r82_x (id int, d xml(document r82_xs) not null, c xml(content r82_xs), \
         p xml(r82_xs), u xml)",
    );
    let ddl = table_ddl(&mut h, &schema, "r82_x");
    let xs = format!("[{schema}].[r82_xs]");
    assert_eq!(
        ddl,
        format!(
            "create table [{schema}].[r82_x] (\n    \
               [id] int null,\n    \
               [d] xml(document {xs}) not null,\n    \
               [c] xml(content {xs}) null,\n    \
               [p] xml(content {xs}) null,\n    \
               [u] xml null\n\
             );\nGO\n"
        )
    );
    let catalog = |h: &mut Harness, owner: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', c.name, x.name, c.is_xml_document) \
                               collate database_default, '; ') \
                      within group (order by c.column_id) \
               from sys.columns c \
               left join sys.xml_schema_collections x on x.xml_collection_id = c.xml_collection_id \
                and c.xml_collection_id > 0 \
              where c.object_id = object_id('[{owner}].[r82_x]')"
        ))
    };
    let before = catalog(&mut h, &schema);
    h.ok("create schema r82_copy_x");
    run_ddl_in(&mut h, &ddl, &schema, "r82_copy_x", &["r82_x"]);
    assert_eq!(catalog(&mut h, "r82_copy_x"), before);
    // The copy validates its XML as the original does.
    for owner in [schema.as_str(), "r82_copy_x"] {
        let t = format!("[{owner}].[r82_x]");
        h.ok(&format!(
            "insert into {t} (id, d, c) values (1, '<a>1</a>', '<a>2</a><a>3</a>')"
        ));
        h.fails(&format!("insert into {t} (id, d) values (2, '<b/>')"));
        h.fails(&format!(
            "insert into {t} (id, d) values (3, '<a>1</a><a>2</a>')"
        ));
    }
    drop_tables(&mut h, "r82_copy_x", "r82_x");
    h.ok("drop schema r82_copy_x");
    drop_tables(&mut h, &schema, "r82_x");
    h.ok("drop xml schema collection r82_xs");
}

/// A table's DDL keeps ROWGUIDCOL, a persisted computed column's NOT NULL,
/// and NOT FOR REPLICATION of identities, foreign keys and checks (#355),
/// which SQL Server never trusts: run elsewhere, the copy's columns and
/// constraints are the original's.
#[test]
fn rowguidcol_not_null_and_not_for_replication() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    drop_tables(&mut h, "r82_copy_r", "r82_r");
    h.ok("drop schema if exists r82_copy_r");
    drop_tables(&mut h, &schema, "r82_r");
    for sql in [
        "create table r82_rp (id int identity(1, 1) not for replication \
           constraint r82_rp_pk primary key, \
         g uniqueidentifier rowguidcol not null constraint r82_rp_g default newid(), \
         a int, c as (a * 2) persisted not null, v as (a + 1) persisted)",
        "create table r82_rc (id int identity(5, 2), pid int, \
         constraint r82_rc_fk foreign key (pid) references r82_rp (id) on delete cascade \
           not for replication, \
         constraint r82_rc_ck check not for replication (id > 0))",
        "alter table r82_rc add constraint r82_rc_off check not for replication (pid <> 0)",
        "alter table r82_rc nocheck constraint r82_rc_off",
    ] {
        h.ok(sql);
    }
    let rp = table_ddl(&mut h, &schema, "r82_rp");
    let t = format!("[{schema}].[r82_rp]");
    assert_eq!(
        rp,
        format!(
            "create table {t} (\n    \
               [id] int identity(1, 1) not for replication not null,\n    \
               [g] uniqueidentifier not null rowguidcol constraint [r82_rp_g] default (newid()),\n    \
               [a] int null,\n    \
               [c] as ([a]*(2)) persisted not null,\n    \
               [v] as ([a]+(1)) persisted,\n    \
               constraint [r82_rp_pk] primary key clustered ([id])\n\
             );\nGO\n"
        )
    );
    let rc = table_ddl(&mut h, &schema, "r82_rc");
    let p = t;
    let t = format!("[{schema}].[r82_rc]");
    assert_eq!(
        rc,
        format!(
            "create table {t} (\n    \
               [id] int identity(5, 2) not null,\n    \
               [pid] int null,\n    \
               constraint [r82_rc_fk] foreign key ([pid]) references {p} ([id]) \
                 on delete cascade not for replication,\n    \
               constraint [r82_rc_ck] check not for replication ([id]>(0))\n\
             );\n\n\
             alter table {t} with nocheck add constraint [r82_rc_off] \
               check not for replication ([pid]<>(0));\n\n\
             alter table {t} nocheck constraint [r82_rc_off];\nGO\n"
        )
    );
    let catalog = |h: &mut Harness, owner: &str| -> Vec<Option<String>> {
        let tables = format!(
            "select object_id from sys.tables where schema_id = schema_id('{owner}') \
                and name like 'r82_r%'"
        );
        [
            format!(
                "select string_agg(concat_ws(' ', object_name(c.object_id), c.name, \
                                             c.is_rowguidcol, c.is_nullable, cc.is_persisted, \
                                             i.is_not_for_replication) \
                                   collate database_default, '; ') \
                          within group (order by object_name(c.object_id), c.column_id) \
                   from sys.columns c \
                   left join sys.computed_columns cc on cc.object_id = c.object_id \
                    and cc.column_id = c.column_id \
                   left join sys.identity_columns i on i.object_id = c.object_id \
                    and i.column_id = c.column_id \
                  where c.object_id in ({tables})"
            ),
            format!(
                "select string_agg(x.line collate database_default, '; ') \
                          within group (order by x.line) \
                   from (select concat_ws(' ', name, is_not_for_replication, is_disabled, \
                                          is_not_trusted, delete_referential_action) as line \
                           from sys.foreign_keys where parent_object_id in ({tables}) \
                         union all \
                         select concat_ws(' ', name, is_not_for_replication, is_disabled, \
                                          is_not_trusted, definition) \
                           from sys.check_constraints where parent_object_id in ({tables})) x"
            ),
        ]
        .iter()
        .map(|sql| h.scalar(sql))
        .collect()
    };
    let before = catalog(&mut h, &schema);
    h.ok("create schema r82_copy_r");
    for ddl in [&rp, &rc] {
        run_ddl_in(&mut h, ddl, &schema, "r82_copy_r", &["r82_rp", "r82_rc"]);
    }
    assert_eq!(catalog(&mut h, "r82_copy_r"), before);
    // The copy takes no NULL for the computed column, and has a $rowguid.
    for owner in [schema.as_str(), "r82_copy_r"] {
        h.ok(&format!("insert into [{owner}].[r82_rp] (a) values (1)"));
        h.fails(&format!("insert into [{owner}].[r82_rp] (a) values (null)"));
        assert_eq!(
            h.scalar(&format!("select count($rowguid) from [{owner}].[r82_rp]"))
                .as_deref(),
            Some("1"),
            "{owner}"
        );
    }
    drop_tables(&mut h, "r82_copy_r", "r82_r");
    h.ok("drop schema r82_copy_r");
    drop_tables(&mut h, &schema, "r82_r");
}

/// A table's DDL makes the statistics made on it, with their filters and
/// options (#357); those of its indexes and the ones SQL Server makes
/// itself are left out. Run elsewhere, the copy has the same statistics.
/// In a database of its own: SQL Server lists the objects of incremental
/// statistics in sys.all_objects with names that aren't UTF-16, which fail
/// other tests' queries of it.
#[test]
fn user_statistics() {
    let Some(params) = dba_params() else {
        return;
    };
    let mut dba = Harness::with(params);
    dba.session.send(DbRequest::SetAutocommit(true));
    let mut h = own_database(&mut dba, "r82_stats", &[]);
    let schema = "dbo";
    let major = h
        .scalar("select cast(serverproperty('ProductMajorVersion') as int)")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_default();
    // AUTO_DROP came with SQL Server 2022.
    let auto_drop = major >= 16;
    for sql in [
        "create partition function r82_st_pf (int) as range right for values (100)",
        "create partition scheme r82_st_ps as partition r82_st_pf all to ([primary])",
        "create table r82_st (id int constraint r82_st_pk primary key, a int, b varchar(10), \
         c int)",
        "create index r82_st_b on r82_st (b)",
        "create statistics r82_st_ab on r82_st (a, b)",
        "create statistics r82_st_f on r82_st (c) where c > 10 with norecompute",
        "create table r82_stp (id int not null, v int) on r82_st_ps (id)",
        "create statistics r82_stp_v on r82_stp (v, id) with incremental = on, norecompute",
    ] {
        h.ok(sql);
    }
    if auto_drop {
        h.ok("create statistics r82_st_ad on r82_st (a) with auto_drop = on");
    }
    let st = table_ddl(&mut h, schema, "r82_st");
    let t = format!("[{schema}].[r82_st]");
    let ad = if auto_drop {
        format!("create statistics [r82_st_ad] on {t} ([a]) with auto_drop = on;\n\n")
    } else {
        String::new()
    };
    assert!(
        st.ends_with(&format!(
            ");\n\n\
             create nonclustered index [r82_st_b] on {t} ([b]);\n\n\
             create statistics [r82_st_ab] on {t} ([a], [b]);\n\n\
             {ad}\
             create statistics [r82_st_f] on {t} ([c]) where ([c]>(10)) with norecompute;\nGO\n"
        )),
        "{st}"
    );
    let stp = table_ddl(&mut h, schema, "r82_stp");
    let t = format!("[{schema}].[r82_stp]");
    assert!(
        stp.ends_with(&format!(
            ") on [r82_st_ps] ([id]);\n\n\
             create statistics [r82_stp_v] on {t} ([v], [id]) \
               with norecompute, incremental = on;\nGO\n"
        )),
        "{stp}"
    );
    let catalog = |h: &mut Harness, owner: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', object_name(s.object_id), s.name, s.user_created, \
                                         s.has_filter, s.filter_definition, s.no_recompute, \
                                         s.is_incremental, c.names) \
                               collate database_default, '; ') \
                      within group (order by object_name(s.object_id), s.name) \
               from sys.stats s \
              outer apply (select string_agg(col_name(sc.object_id, sc.column_id), ',') \
                                    within group (order by sc.stats_column_id) as names \
                             from sys.stats_columns sc \
                            where sc.object_id = s.object_id and sc.stats_id = s.stats_id) c \
              where s.object_id in (select object_id from sys.tables \
                                     where schema_id = schema_id('{owner}') \
                                       and name like 'r82_st%') \
                and s.name not like '[_]WA[_]Sys[_]%' \
                and s.name not like 'PK[_][_]%'"
        ))
    };
    let before = catalog(&mut h, schema);
    h.ok("create schema r82_copy_st");
    for ddl in [&st, &stp] {
        run_ddl_in(&mut h, ddl, schema, "r82_copy_st", &["r82_st", "r82_stp"]);
    }
    assert_eq!(catalog(&mut h, "r82_copy_st"), before);
    if auto_drop {
        assert_eq!(
            h.scalar(
                "select auto_drop from sys.stats \
                  where object_id = object_id('r82_copy_st.r82_st') and name = 'r82_st_ad'"
            )
            .as_deref(),
            Some("1")
        );
    }
    drop(h);
    drop_database(&mut dba, "r82_stats");
}

/// The graph tables of `owner` whose names start with `prefix`, as the
/// catalog has them: node or edge, their columns, graph ones by their
/// kind, their indexes, a node table's own unique index without its
/// number, and their edge constraints.
fn catalog_graph(h: &mut Harness, owner: &str, prefix: &str) -> Vec<Option<String>> {
    let tables = format!(
        "select object_id from sys.tables where schema_id = schema_id('{owner}') \
            and name like '{prefix}%'"
    );
    [
        format!(
            "select string_agg(concat_ws(' ', t.name, t.is_node, t.is_edge) \
                               collate database_default, '; ') within group (order by t.name) \
               from sys.tables t where t.object_id in ({tables})"
        ),
        format!(
            "select string_agg(concat_ws(' ', object_name(c.object_id), \
                                         coalesce(c.graph_type_desc, c.name), \
                                         type_name(c.user_type_id), c.is_nullable, c.is_hidden) \
                               collate database_default, '; ') \
                      within group (order by object_name(c.object_id), c.column_id) \
               from sys.columns c where c.object_id in ({tables})"
        ),
        format!(
            "select string_agg(concat_ws(' ', object_name(i.object_id), \
                                         case when i.name like 'GRAPH[_]UNIQUE[_]INDEX[_]%' \
                                              then 'GRAPH_UNIQUE_INDEX' else i.name end, \
                                         i.type_desc, i.is_unique, i.is_primary_key, cols.names) \
                               collate database_default, '; ') \
                      within group (order by object_name(i.object_id), i.name) \
               from sys.indexes i \
              outer apply (select string_agg((coalesce(c.graph_type_desc, c.name) \
                                              + case when ic.is_descending_key = 1 \
                                                     then ' desc' else '' end) \
                                             collate database_default, ',') \
                                    within group (order by ic.key_ordinal) as names \
                             from sys.index_columns ic \
                             join sys.columns c on c.object_id = ic.object_id \
                              and c.column_id = ic.column_id \
                            where ic.object_id = i.object_id and ic.index_id = i.index_id) cols \
              where i.object_id in ({tables}) and i.type > 0"
        ),
        format!(
            "select string_agg(concat_ws(' ', object_name(e.parent_object_id), e.name, \
                                         e.delete_referential_action, e.is_disabled, \
                                         e.is_not_trusted, cl.clauses) \
                               collate database_default, '; ') within group (order by e.name) \
               from sys.edge_constraints e \
              outer apply (select string_agg(object_name(c.from_object_id) + '>' \
                                             + object_name(c.to_object_id), ',') \
                                    within group (order by c.clause_number) as clauses \
                             from sys.edge_constraint_clauses c \
                            where c.object_id = e.object_id) cl \
              where e.parent_object_id in ({tables})"
        ),
    ]
    .iter()
    .map(|sql| h.scalar(sql))
    .collect()
}

/// A graph table's DDL makes a node or edge table (#360), without the
/// columns SQL Server adds to it and a node table's own unique index, with
/// indexes and statistics on `$node_id`, `$from_id` and `$to_id`, and an
/// edge table's edge constraints: run elsewhere, the copy is the same kind
/// of table, which MATCH reads.
#[test]
fn graph_tables() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    let major = h
        .scalar("select cast(serverproperty('ProductMajorVersion') as int)")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_default();
    // Graph tables came with SQL Server 2017, edge constraints with 2019.
    if major < 15 {
        return;
    }
    let drop = |h: &mut Harness, owner: &str| {
        h.ok(&format!(
            "drop table if exists [{owner}].[r82_ge], [{owner}].[r82_gk]"
        ));
        drop_tables(h, owner, "r82_g");
    };
    drop(&mut h, "r82_copy_g");
    h.ok("drop schema if exists r82_copy_g");
    drop(&mut h, &schema);
    for sql in [
        "create table r82_gp (id int constraint r82_gp_pk primary key, name varchar(20)) as node",
        "create table r82_gn (x int, constraint r82_gn_pk primary key nonclustered ($node_id)) \
         as node",
        "create index r82_gn_x on r82_gn ($node_id desc, x)",
        "create table r82_ge (rating int, \
         constraint r82_ge_ec connection (r82_gp to r82_gp, r82_gp to r82_gn) on delete cascade) \
         as edge",
        "create index r82_ge_ft on r82_ge ($from_id, $to_id desc)",
        "create statistics r82_ge_s on r82_ge ($to_id, rating)",
        "create table r82_gk as edge",
        "alter table r82_gk add constraint r82_gk_ec connection (r82_gn to r82_gn)",
        "alter table r82_gk nocheck constraint r82_gk_ec",
    ] {
        h.ok(sql);
    }
    let (gp, gn, ge, gk) = (
        format!("[{schema}].[r82_gp]"),
        format!("[{schema}].[r82_gn]"),
        format!("[{schema}].[r82_ge]"),
        format!("[{schema}].[r82_gk]"),
    );
    let ddls: Vec<String> = ["r82_gp", "r82_gn", "r82_ge", "r82_gk"]
        .iter()
        .map(|table| table_ddl(&mut h, &schema, table))
        .collect();
    assert_eq!(
        ddls,
        [
            format!(
                "create table {gp} (\n    \
                   [id] int not null,\n    \
                   [name] varchar(20) null,\n    \
                   constraint [r82_gp_pk] primary key clustered ([id])\n\
                 ) as node;\nGO\n"
            ),
            format!(
                "create table {gn} (\n    \
                   [x] int null,\n    \
                   constraint [r82_gn_pk] primary key nonclustered ($node_id)\n\
                 ) as node;\n\n\
                 create nonclustered index [r82_gn_x] on {gn} ($node_id desc, [x]);\nGO\n"
            ),
            format!(
                "create table {ge} (\n    \
                   [rating] int null,\n    \
                   constraint [r82_ge_ec] connection ({gp} to {gp}, {gp} to {gn}) \
                     on delete cascade\n\
                 ) as edge;\n\n\
                 create nonclustered index [r82_ge_ft] on {ge} ($from_id, $to_id desc);\n\n\
                 create statistics [r82_ge_s] on {ge} ($to_id, [rating]);\nGO\n"
            ),
            format!(
                "create table {gk} as edge;\n\n\
                 alter table {gk} with nocheck add constraint [r82_gk_ec] \
                   connection ({gn} to {gn});\n\n\
                 alter table {gk} nocheck constraint [r82_gk_ec];\nGO\n"
            ),
        ]
    );
    let before = catalog_graph(&mut h, &schema, "r82_g");
    assert_eq!(before.len(), 4, "{before:?}");
    h.ok("create schema r82_copy_g");
    let tables = ["r82_gp", "r82_gn", "r82_ge", "r82_gk"];
    for ddl in &ddls {
        run_ddl_in(&mut h, ddl, &schema, "r82_copy_g", &tables);
    }
    assert_eq!(catalog_graph(&mut h, "r82_copy_g", "r82_g"), before);
    // The copy's nodes and edges make a graph.
    h.ok("insert into r82_copy_g.r82_gp values (1, 'a'), (2, 'b')");
    h.ok("insert into r82_copy_g.r82_ge ($from_id, $to_id, rating) \
         select a.$node_id, b.$node_id, 5 from r82_copy_g.r82_gp a, r82_copy_g.r82_gp b \
          where a.id = 1 and b.id = 2");
    assert_eq!(
        h.scalar(
            "select b.name from r82_copy_g.r82_gp a, r82_copy_g.r82_ge e, r82_copy_g.r82_gp b \
              where match(a-(e)->b) and a.id = 1"
        )
        .as_deref(),
        Some("b")
    );
    drop(&mut h, "r82_copy_g");
    h.ok("drop schema r82_copy_g");
    drop(&mut h, &schema);
}

/// Object filters and Find find names without case, non-ASCII letters too,
/// in the test database and, as a DBA, in one with a case-sensitive
/// collation.
#[test]
fn filters_match_any_letters_without_case() {
    let Some(params) = params() else {
        return;
    };
    let check = |h: &mut Harness| {
        let schema = h.schema.clone();
        let tables = "[r62_čaj], [r62_bière], [r62_ÖL], [R62_Case], [r62_straße]";
        h.ok(&format!("drop table if exists {tables}"));
        for table in tables.split(", ") {
            h.ok(&format!("create table {table} (x int)"));
        }
        let mut filtered = |filter: &str| -> Vec<String> {
            let MetaResult::Objects(objects) = h.meta(MetaQuery::Objects {
                owner: schema.clone(),
                object_type: ObjectType::Table,
                filter: Some(filter.into()),
            }) else {
                panic!("expected objects")
            };
            objects.into_iter().map(|o| o.name).collect()
        };
        assert_eq!(filtered("r62_čaj"), ["r62_čaj"]);
        assert_eq!(filtered("R62_ČAJ"), ["r62_čaj"]);
        assert_eq!(filtered("r62_BIÈRE"), ["r62_bière"]);
        assert_eq!(filtered("r62_öl"), ["r62_ÖL"]);
        assert_eq!(filtered("r62_ca"), ["R62_Case"]);
        assert_eq!(filtered("R62_CASE"), ["R62_Case"]);
        assert_eq!(filtered("r62_straße"), ["r62_straße"]);
        assert_eq!(filtered("r62_?ase"), ["R62_Case"]);
        assert_eq!(filtered("r62%"), Vec::<String>::new());
        let MetaResult::Objects(found) = h.meta(MetaQuery::Find {
            pattern: "R62_ČAJ".into(),
            limit: 10,
        }) else {
            panic!("expected objects")
        };
        let found: Vec<&str> = found.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(found, ["r62_čaj"]);
        h.ok(&format!("drop table {tables}"));
    };
    let mut h = Harness::with(params.clone());
    h.session.send(DbRequest::SetAutocommit(true));
    check(&mut h);
    let Some(dba) = std::env::var("PYTHIA_TEST_MSSQL_DBA")
        .ok()
        .and_then(|spec| from_spec(&spec))
    else {
        return;
    };
    let mut d = Harness::with(dba.clone());
    d.session.send(DbRequest::SetAutocommit(true));
    d.ok("drop database if exists r62_cs");
    d.ok("create database r62_cs collate Latin1_General_100_CS_AS_SC");
    let connect_string = {
        let (server, _) = dba.connect_string.split_once('/').expect("a database");
        let options = dba.connect_string.split_once('?').map_or("", |(_, o)| o);
        format!("{server}/r62_cs?{options}")
    };
    let mut c = Harness::with(ConnectParams {
        connect_string,
        ..dba
    });
    c.session.send(DbRequest::SetAutocommit(true));
    assert_eq!(
        c.scalar("select convert(varchar(60), databasepropertyex(db_name(), 'Collation'))")
            .as_deref(),
        Some("Latin1_General_100_CS_AS_SC")
    );
    check(&mut c);
    drop(c);
    d.ok("drop database r62_cs");
}

/// Waits for a session's connect to fail.
fn connect_error(params: ConnectParams) -> pythia_db::DbError {
    let (tx, wake) = mpsc::channel();
    let session = Session::connect(params, move || {
        let _ = tx.send(());
    });
    loop {
        match session.try_recv() {
            Some(DbEvent::ConnectFailed(error)) => return error,
            Some(other) => panic!("expected ConnectFailed, got {other:?}"),
            None => wake
                .recv_timeout(Duration::from_secs(60))
                .expect("timed out waiting for the connect"),
        }
    }
}

/// The database's users and roles; `sys` and `INFORMATION_SCHEMA` aren't
/// users.
#[test]
fn accounts() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let accounts = |h: &mut Harness, object_type| {
        let MetaResult::Accounts(accounts) = h.meta(MetaQuery::Accounts(object_type)) else {
            panic!("expected accounts")
        };
        accounts
    };
    let names = |accounts: &[(String, String)]| -> Vec<String> {
        accounts.iter().map(|(name, _)| name.clone()).collect()
    };
    h.ok("drop user if exists meta_user");
    h.ok("drop role if exists meta_role");
    h.ok("create user meta_user without login");
    h.ok("create role meta_role");

    let users = names(&accounts(&mut h, ObjectType::User));
    assert!(users.contains(&"meta_user".to_owned()), "{users:?}");
    assert!(users.contains(&"dbo".to_owned()), "{users:?}");
    assert!(!users.contains(&"sys".to_owned()), "{users:?}");
    assert!(!users.contains(&"meta_role".to_owned()), "{users:?}");
    let roles = names(&accounts(&mut h, ObjectType::Role));
    assert!(roles.contains(&"meta_role".to_owned()), "{roles:?}");
    assert!(roles.contains(&"db_owner".to_owned()), "{roles:?}");
    assert!(!roles.contains(&"meta_user".to_owned()), "{roles:?}");

    h.ok("drop user meta_user");
    h.ok("drop role meta_role");
}

/// Through the SSH server (#97): trusting its key, the login over the
/// tunnel (encrypted, as SQL Server's is by default), sessions sharing it,
/// Break (the driver connects again, through the tunnel), a named instance,
/// and an address the SSH server can't reach.
#[test]
fn through_an_ssh_tunnel() {
    let dir = std::env::temp_dir().join(format!("pythia-mssql-ssh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let known_hosts = dir.join("known_hosts");
    let Some(params) = ssh_params(&known_hosts) else {
        return;
    };

    // The first connect asks to trust the server.
    let error = connect_error(params.clone());
    let key = error.host_key.expect("an unknown host key");
    key.trust().unwrap();

    let mut h = Harness::with(params.clone());
    assert_eq!(h.scalar("select 6 * 7").as_deref(), Some("42"));
    let mut other = Harness::with(params.clone());
    assert_eq!(other.scalar("select db_name()").as_deref(), Some("pythia"));

    h.execute(1, "waitfor delay '00:00:30'; select 1", 10);
    std::thread::sleep(Duration::from_millis(500));
    h.session.cancel();
    let error = loop {
        if let DbEvent::Failed { error, .. } = h.next() {
            break error;
        }
    };
    assert_eq!(error.message, "Cancelled");
    h.settle();
    assert_eq!(h.scalar("select 'again'").as_deref(), Some("again"));

    // The SQL Server Browser can't be reached through the tunnel.
    let named = ConnectParams {
        connect_string: "mssql\\sqlexpress/pythia?trust".into(),
        ..params.clone()
    };
    let error = connect_error(named);
    assert!(error.message.contains("host:port"), "{error}");

    // An address the SSH server can't reach: the tunnel says why.
    let unreachable = ConnectParams {
        connect_string: "nosuchhost.invalid:1433/pythia?trust".into(),
        ..params
    };
    let error = connect_error(unreachable);
    assert!(
        error
            .message
            .contains("the server couldn't connect to nosuchhost.invalid:1433"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Megabytes with every byte value, and text that isn't ASCII: what a
/// statement binds whole (#293), and the same reversed.
fn whole_values(seed: usize) -> (Vec<u8>, String) {
    let mut bytes: Vec<u8> = (0..3usize << 20)
        .map(|i| ((i * 7 + i / 256 + seed) % 256) as u8)
        .collect();
    let mut text: String = (0..20_000)
        .map(|i| format!("Zeile {i}: Grüße aus Zürich, 日本語のテキスト, Ελληνικά, 😀\n"))
        .collect();
    if seed > 0 {
        bytes.reverse();
        text = text.chars().rev().collect();
    }
    (bytes, text)
}

/// The bytes of a binary value fetched whole as hex (after `0x` or `\x`).
fn unhex(text: &str) -> Vec<u8> {
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("\\x"))
        .unwrap_or(text);
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// `name` bound to the bytes or the text of `values`.
fn binds(values: &(Vec<u8>, String), names: &[&str]) -> Vec<(String, Bind)> {
    names
        .iter()
        .map(|&name| {
            let value = match name {
                "b" => Bind::Bytes(values.0.as_slice().into()),
                _ => Bind::LongText(values.1.as_str().into()),
            };
            (name.to_owned(), value)
        })
        .collect()
}

impl Harness {
    /// Runs a statement with `binds`; its rows affected, or the error.
    fn bound(&mut self, sql: &str, binds: Vec<(String, Bind)>) -> Result<Option<u64>, String> {
        self.session.send(DbRequest::Execute {
            id: 7,
            sql: sql.into(),
            max_rows: 10,
            binds,
        });
        match self.next_result() {
            DbEvent::Executed { summary, .. } => Ok(summary.rows_affected),
            DbEvent::Failed { error, .. } => Err(error.text()),
            other => panic!("{sql}: {other:?}"),
        }
    }

    /// The first row of a query, its values whole.
    fn whole_row(&mut self, sql: &str) -> Vec<Option<String>> {
        self.session.send(DbRequest::ExecuteWhole {
            id: 8,
            sql: sql.into(),
            max_rows: 10,
        });
        assert!(matches!(self.next_result(), DbEvent::QueryStarted { .. }));
        let DbEvent::Rows { mut rows, .. } = self.next_result() else {
            panic!("{sql}: expected rows")
        };
        rows.remove(0)
    }
}

/// Bytes bind as varbinary(max) and text as nvarchar(max), whole: megabytes
/// into a varbinary(max), an image, an nvarchar(max) and an ntext, inserted
/// and updated (#293).
#[test]
fn values_bound_whole() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists lob_bound");
    h.ok("create table lob_bound (id int primary key, b varbinary(max), i image, t nvarchar(max), n ntext)");
    let (first, second) = (whole_values(0), whole_values(1));
    let insert = "insert into lob_bound (id, b, i, t, n) values (1, :b, :b, :t, :t)";
    assert_eq!(h.bound(insert, binds(&first, &["b", "t"])), Ok(Some(1)));
    let update = "update lob_bound set b = :b, i = :b, t = :t, n = :t where id = 1";
    assert_eq!(h.bound(update, binds(&second, &["b", "t"])), Ok(Some(1)));
    let row = h.whole_row("select b, i, t, n from lob_bound");
    assert!(unhex(row[0].as_deref().unwrap()) == second.0, "b");
    assert!(unhex(row[1].as_deref().unwrap()) == second.0, "i");
    assert!(row[2].as_deref() == Some(second.1.as_str()), "t");
    assert!(row[3].as_deref() == Some(second.1.as_str()), "n");
    // The changes' transaction would keep the table.
    h.ok("rollback");
    h.ok("drop table if exists lob_bound");
}
