//! PostgreSQL sessions against a real server.
//!
//! Skipped unless `PYTHIA_TEST_POSTGRES` is set to `user/password@host[:port]/database`,
//! e.g. the PostgreSQL container from `docker-compose.yml`:
//! `PYTHIA_TEST_POSTGRES=pythia/pythia@localhost:5432/pythia cargo test -p pythia-db --test postgres`
//!
//! The SSH tunnel test also needs `PYTHIA_TEST_SSH=user:password@host[:port]`
//! and `PYTHIA_TEST_POSTGRES_VIA_SSH`, the database as the SSH server sees
//! it (the `ssh` service: `pythia:pythia@localhost:2222` and
//! `pythia/pythia@postgres:5432/pythia`).

use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use pythia_db::ssh::{SshParams, Tunnel};
use pythia_db::{
    Bind, ConnectParams, DbEvent, DbKind, DbRequest, Detail, Dialect, MetaQuery, MetaResult,
    ObjectType, Session,
};

fn params() -> Option<ConnectParams> {
    from_spec(&std::env::var("PYTHIA_TEST_POSTGRES").ok()?)
}

/// The database through the SSH server, trusting the host keys kept in
/// `known_hosts`.
fn ssh_params(known_hosts: &Path) -> Option<ConnectParams> {
    let ssh = std::env::var("PYTHIA_TEST_SSH").ok()?;
    let mut params = from_spec(&std::env::var("PYTHIA_TEST_POSTGRES_VIA_SSH").ok()?)?;
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
        kind: DbKind::Postgres,
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
                assert_eq!(server.dialect, Dialect::POSTGRES);
                assert!(server.release >= (12, 0), "{server:?}");
                assert!(server.banner.starts_with("PostgreSQL "), "{server:?}");
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

#[test]
fn query_and_fetch_more() {
    let Some(mut h) = Harness::connect() else {
        eprintln!("PYTHIA_TEST_POSTGRES not set, skipping");
        return;
    };
    h.execute(
        1,
        "select n, 'row ' || n as label from generate_series(1, 25) n",
        10,
    );
    let DbEvent::QueryStarted { columns, .. } = h.next_result() else {
        panic!("expected QueryStarted")
    };
    assert_eq!(
        columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["n", "label"]
    );
    assert_eq!(columns[0].type_name, "integer");
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
        "select 1.50::numeric(5,2) d, date '2024-02-29' dt, \
                timestamp '2024-02-29 13:45:01' ts, null n, '\\x00ff'::bytea b, 'grüße' s, \
                true t, array[1, 2] a",
    ) else {
        panic!("expected QueryStarted")
    };
    let types: Vec<&str> = columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(
        types,
        [
            "numeric",
            "date",
            "timestamp",
            "text",
            "bytea",
            "text",
            "boolean",
            "integer[]"
        ]
    );
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(
        rows[0],
        vec![
            Some("1.50".into()),
            Some("2024-02-29".into()),
            Some("2024-02-29 13:45:01".into()),
            None,
            Some("\\x00ff".into()),
            Some("grüße".into()),
            Some("t".into()),
            Some("{1,2}".into()),
        ]
    );
    h.settle();

    h.session.send(DbRequest::Execute {
        id: 2,
        sql: "select :a::int + 1 as x, :B is null as y, :a as again, 'it''s :a' as s".into(),
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
        vec![
            Some("42".into()),
            Some("t".into()),
            Some("41".into()),
            Some("it's :a".into())
        ]
    );
}

#[test]
fn errors_carry_sqlstate_and_position() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let DbEvent::Failed { error, .. } = h.run(1, "select * from no_such_table") else {
        panic!("expected Failed")
    };
    assert!(
        error
            .message
            .starts_with("ERROR 42P01: relation \"no_such_table\" does not exist"),
        "{}",
        error.message
    );
    assert_eq!(error.offset, Some(14));
    assert!(!error.is_connection_lost());
    h.settle();

    let sql = "select *\n  frm t";
    let DbEvent::Failed { error, .. } = h.run(2, sql) else {
        panic!("expected Failed")
    };
    assert_eq!(error.offset, Some(11), "{}", error.message);
}

/// Scripts split where PostgreSQL ends their statements: a dollar-quoted
/// body may start with a word (#201), and `E'…'` takes backslash
/// escapes (#202).
#[test]
fn scripts_split_as_the_server_reads_them() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let script = "create or replace function fs_three() returns int language plpgsql \
                  as $$BEGIN RETURN 3; END$$;\n\
                  select $$abc$$ || fs_three() as e;\n\
                  select E'it\\'s; here' as e;\n\
                  select 5;";
    let statements: Vec<&str> =
        pythia_plsql::split_statements_in(script, Dialect::POSTGRES.syntax())
            .iter()
            .map(|s| s.text(script))
            .collect();
    assert_eq!(statements.len(), 4, "{statements:?}");
    h.ok(statements[0]);
    assert_eq!(h.scalar(statements[1]).as_deref(), Some("abc3"));
    assert_eq!(h.scalar(statements[2]).as_deref(), Some("it's; here"));
    assert_eq!(h.scalar(statements[3]).as_deref(), Some("5"));
    h.ok("drop function fs_three()");
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
    assert!(h.fails("select no_such_column from tx_t"));
    assert_eq!(h.scalar("select count(*) from tx_t").as_deref(), Some("1"));
    assert!(h.ok("insert into tx_t values (6)"));
    h.session.send(DbRequest::Commit);
    assert!(!h.settle());
    assert_eq!(h.scalar("select count(*) from tx_t").as_deref(), Some("2"));

    // The session's own savepoints work as written.
    assert!(h.ok("begin"));
    h.ok("insert into tx_t values (7)");
    h.ok("savepoint s");
    h.ok("insert into tx_t values (8)");
    h.ok("rollback to savepoint s");
    assert!(!h.ok("commit"));
    assert_eq!(h.scalar("select max(a) from tx_t").as_deref(), Some("7"));

    h.session.send(DbRequest::SetAutocommit(true));
    let DbEvent::Executed { summary, .. } = h.run(2, "insert into tx_t values (2), (3)") else {
        panic!("expected Executed")
    };
    assert_eq!(summary.rows_affected, Some(2));
    assert_eq!(summary.to_string(), "INSERT: 2 rows affected");
    assert!(!h.settle(), "autocommit committed the insert");
    h.ok("drop table tx_t");
}

/// Rows affected are the rows the statement matched, also when it sets a
/// value a row already has, and not those a trigger changes; inside a
/// transaction (after the statement's savepoint) too. Editing a result
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
        "create function count_f() returns trigger language plpgsql as \
         $$ begin update count_log set n = n + 1; return null; end $$",
    );
    h.ok("create trigger count_tr after update on count_t for each row execute function count_f()");
    h.ok("insert into count_t values (1, 10), (2, 20)");
    h.ok("insert into count_log values (1), (2), (3)");
    let affected = |h: &mut Harness, sql: &str| {
        let DbEvent::Executed { summary, .. } = h.run(1, sql) else {
            panic!("{sql}: expected Executed")
        };
        h.settle();
        summary.rows_affected
    };
    let matched = affected(&mut h, "update count_t set x = x where id = 1");
    assert_eq!(matched, Some(1));
    // Now in a transaction: the next ones run after a savepoint.
    let none = affected(&mut h, "update count_t set x = 5 where id = 3");
    assert_eq!(none, Some(0));
    assert_eq!(affected(&mut h, "update count_t set x = x"), Some(2));
    let deleted = affected(&mut h, "delete from count_t where id = 2");
    assert_eq!(deleted, Some(1));
    h.session.send(DbRequest::Rollback);
    h.settle();
    h.ok("drop table count_t, count_log");
    h.ok("drop function count_f");
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
    b.ok("alter table fresh_t add column b int");
    a.ok("drop table fresh_t");
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
                format!(
                    "failed {}",
                    error.message.split(':').next().unwrap_or_default()
                )
            }
            other => format!("{other:?}"),
        });
        // Waiting for a FetchMore: no transaction state comes yet.
        if out.last().is_some_and(|e| e.ends_with(" true")) {
            return out;
        }
    }
}

/// Every result of a statement is a result of its own (#316): those of a
/// text of several statements, and the SELECTs rules add to an INSERT,
/// whose rows showed as one result under the first one's columns. An
/// error after a result fails the statement.
#[test]
fn several_results_of_one_statement() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists r66_r");
    h.ok("create table r66_r (id int)");
    h.ok("create rule r66_a as on insert to r66_r do also select 1 as a");
    h.ok("create rule r66_b as on insert to r66_r do also select 'x' as b, 2 as c");
    let id = 66;
    let run = |h: &mut Harness, sql: &str, max_rows: usize| {
        h.execute(id, sql, max_rows);
        outline(h, id)
    };
    let more = "MoreResults { id: 66 }";
    let two = ["cols a", more, "rows 1 false", "cols b c", "rows 1 false"];
    assert_eq!(run(&mut h, "insert into r66_r values (1)", 10), two);
    assert_eq!(
        run(&mut h, "select 1 as a; select 'x' as b, 2 as c", 10),
        two
    );
    assert_eq!(
        run(
            &mut h,
            "create temp table r66_t (id int); select 3 as d",
            10
        ),
        ["cols d", "rows 1 false"]
    );
    assert_eq!(
        run(&mut h, "select 1 as a; select * from r66_no_such_table", 10),
        ["cols a", more, "rows 1 false", "failed ERROR 42P01"]
    );
    // A result fetched batch by batch: the next one comes after its last.
    let paged = "select n from generate_series(1, 15) n; select 'end' as e";
    assert_eq!(run(&mut h, paged, 10), ["cols n", "rows 10 true"]);
    h.session.send(DbRequest::FetchMore { id, max_rows: 10 });
    assert_eq!(
        outline(&mut h, id),
        [more, "rows 5 false", "cols e", "rows 1 false"]
    );
    // One result, as before, with its types.
    h.execute(id, "select 1 as a", 10);
    let DbEvent::QueryStarted { columns, .. } = h.next_result() else {
        panic!("expected QueryStarted")
    };
    assert!(columns[0].numeric);
    assert_eq!(outline(&mut h, id), ["rows 1 false"]);
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
        ["The statement returned 2 more result(s); only the first was written to the file"]
    );
    h.ok("drop table r66_r");
}

#[test]
fn closing_an_open_result_stops_the_server() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    // A hundred million rows, streamed (in `from`, the function's rows
    // would be stored first); only ten are fetched.
    h.execute(1, "select generate_series(1, 100000000) as n", 10);
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));

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
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    // Fetching more of the closed result says it is closed.
    h.session.send(DbRequest::FetchMore {
        id: 1,
        max_rows: 10,
    });
    assert!(matches!(h.next_result(), DbEvent::QueryClosed { id: 1 }));

    // A small result that is closed early needs no cancel.
    h.execute(2, "select n from generate_series(1, 30) n", 10);
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));
    h.session.send(DbRequest::Commit);
    assert!(matches!(h.next_result(), DbEvent::QueryClosed { id: 2 }));

    // The cancel didn't hit a later statement.
    for i in 0..3 {
        assert_eq!(h.scalar(&format!("select {i}")), Some(i.to_string()));
    }
}

#[test]
fn cancel_interrupts_a_running_statement() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.execute(1, "select pg_sleep(30)", 10);
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    h.session.cancel();
    let error = loop {
        match h.next_result() {
            DbEvent::QueryStarted { .. } => {}
            DbEvent::Failed { error, .. } => break error,
            other => panic!("expected Failed, got {other:?}"),
        }
    };
    assert!(error.message.contains("57014"), "{}", error.message);
    assert!(started.elapsed() < Duration::from_secs(5));
    h.settle();
    h.ok("select 1");
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
        "This tool isn't available for PostgreSQL connections"
    );
}

/// The login statements may set the search path (the profile's default
/// schema): `Connected` reports the schema they leave the session in, then
/// their failures (#103).
#[test]
fn connected_reports_the_schema_after_the_login_statements() {
    let Some(mut params) = params() else {
        return;
    };
    params.init_sql = vec![
        "set search_path to pg_catalog".into(),
        "select no_such_column".into(),
    ];
    let (tx, wake) = mpsc::channel();
    let session = Session::connect(params, move || {
        let _ = tx.send(());
    });
    let mut h = Harness {
        session,
        wake,
        schema: String::new(),
    };
    let DbEvent::Connected(server) = h.next() else {
        panic!("expected Connected")
    };
    assert_eq!(server.current_schema, "pg_catalog");
    let DbEvent::Notice(notice) = h.next() else {
        panic!("expected the login script's failure")
    };
    assert!(notice.starts_with("Login script: "), "{notice}");
}

/// The search paths a statement reported (#320), up to the transaction
/// state that ends its events; it must succeed.
fn paths(h: &mut Harness, sql: &str) -> Vec<Vec<String>> {
    h.execute(0, sql, 100);
    let mut paths = Vec::new();
    loop {
        match h.next() {
            DbEvent::Context(context) => paths.push(context.schemas),
            DbEvent::Failed { error, .. } => panic!("{sql}: {error}"),
            DbEvent::Transaction { .. } => return paths,
            _ => {}
        }
    }
}

/// Every way a statement changes the search path is reported, for
/// completion to look names up in it (#320): set_config as SET does, for
/// the path and for the role that "$user" names; a SET LOCAL until its
/// transaction ends. A function's own SET search_path is undone when it
/// returns. Before, a set_config reported nothing.
#[test]
fn the_search_path_is_reported_as_it_changes() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let s = |names: &[&str]| -> Vec<Vec<String>> {
        vec![names.iter().map(|n| (*n).to_owned()).collect()]
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r85_s cascade");
    h.ok("drop schema if exists r85_role cascade");
    h.ok("drop role if exists r85_role");
    h.ok("create schema r85_s");
    h.ok("create role r85_role");
    h.ok("create schema r85_role authorization r85_role");
    h.ok("grant r85_role to current_user");
    h.ok("create function r85_s.r85_own() returns int language sql \
         set search_path = r85_s as $$ select 1 $$");
    h.ok("set search_path = public");

    assert_eq!(
        paths(
            &mut h,
            "select set_config('search_path', 'r85_s, public', false)"
        ),
        s(&["r85_s", "public"])
    );
    assert_eq!(
        paths(
            &mut h,
            "SELECT pg_catalog.SET_CONFIG('search_path', 'public', false)"
        ),
        s(&["public"])
    );
    // "$user" is the role's schema once the role is set.
    h.ok("set search_path = \"$user\", public");
    assert_eq!(
        paths(&mut h, "select set_config('role', 'r85_role', false)"),
        s(&["r85_role", "public"])
    );
    assert_eq!(paths(&mut h, "reset role"), s(&["public"]));
    // A function's SET search_path is its own.
    assert!(paths(&mut h, "select r85_s.r85_own()").is_empty());
    assert_eq!(
        h.scalar("select current_schemas(false)::text").as_deref(),
        Some("{public}")
    );
    // A local setting lasts until its transaction ends.
    h.session.send(DbRequest::SetAutocommit(false));
    h.ok("begin");
    assert_eq!(
        paths(&mut h, "select set_config('search_path', 'r85_s', true)"),
        s(&["r85_s"])
    );
    assert_eq!(paths(&mut h, "commit"), s(&["public"]));
    h.ok("begin");
    assert_eq!(
        paths(&mut h, "set local search_path = r85_s"),
        s(&["r85_s"])
    );
    h.session.send(DbRequest::Rollback);
    let mut reported = Vec::new();
    loop {
        match h.next() {
            DbEvent::Context(context) => reported.push(context.schemas),
            DbEvent::Transaction { .. } => break,
            _ => {}
        }
    }
    assert_eq!(reported, s(&["public"]));

    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema r85_s cascade");
    h.ok("drop schema r85_role cascade");
    h.ok("drop role r85_role");
}

#[test]
fn notices_become_messages() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.execute(1, "do $$ begin raise notice 'hello %', 6 * 7; end $$", 10);
    let mut notices = Vec::new();
    loop {
        match h.next() {
            DbEvent::Notice(text) => notices.push(text),
            DbEvent::Failed { error, .. } => panic!("{error}"),
            DbEvent::Transaction { .. } => break,
            _ => {}
        }
    }
    assert!(
        notices.contains(&"NOTICE: hello 42".to_owned()),
        "{notices:?}"
    );
    // DO opened a transaction of its own.
    h.session.send(DbRequest::Rollback);
    h.settle();
}

#[test]
fn dictionary() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let schema = h.schema.clone();
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop view if exists meta_v");
    h.ok("drop table if exists meta_emp");
    h.ok("drop table if exists meta_dept");
    h.ok("drop function if exists meta_f(int)");
    h.ok("create table meta_dept (deptno int primary key, name varchar(30) not null)");
    h.ok("comment on column meta_dept.name is 'Department'");
    h.ok(
        "create table meta_emp (empno int generated always as identity primary key, deptno int \
         constraint meta_emp_fk references meta_dept (deptno), \"Salary\" numeric(8,2) default 0)",
    );
    h.ok("create index meta_emp_dept on meta_emp (deptno)");
    h.ok("create view meta_v as select e.empno, d.name from meta_emp e join meta_dept d using (deptno)");
    h.ok(
        "create function meta_f(p int) returns int language plpgsql as $$ begin return p + 1; end $$",
    );

    let MetaResult::Schemas(schemas) = h.meta(MetaQuery::Schemas) else {
        panic!("expected schemas")
    };
    assert!(schemas.contains(&schema), "{schemas:?}");

    let MetaResult::Objects(tables) = h.meta(MetaQuery::Objects {
        owner: schema.clone(),
        object_type: ObjectType::Table,
        filter: Some("META_".into()),
    }) else {
        panic!("expected objects")
    };
    let names: Vec<&str> = tables.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["meta_dept", "meta_emp"]);

    let MetaResult::Objects(functions) = h.meta(MetaQuery::Objects {
        owner: schema.clone(),
        object_type: ObjectType::Function,
        filter: Some("meta".into()),
    }) else {
        panic!("expected objects")
    };
    assert_eq!(functions.len(), 1, "{functions:?}");

    // Code assistance asks in uppercase.
    let MetaResult::Columns(columns) = h.meta(MetaQuery::Columns {
        owner: schema.clone(),
        table: "META_DEPT".into(),
        keys: true,
    }) else {
        panic!("expected columns")
    };
    assert_eq!(columns.len(), 2);
    assert_eq!(
        (columns[0].name.as_str(), columns[0].pk_position),
        ("deptno", Some(1))
    );
    assert_eq!(columns[1].data_type, "character varying(30)");
    assert!(!columns[1].nullable);
    assert_eq!(columns[1].comment.as_deref(), Some("Department"));

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: "meta_emp".into(),
        detail: Detail::Constraints,
    }) else {
        panic!("expected a table")
    };
    let fk = rows
        .iter()
        .find(|r| r[0].as_deref() == Some("meta_emp_fk"))
        .expect("the foreign key");
    assert_eq!(fk[1].as_deref(), Some("Foreign key"));
    assert_eq!(fk[3].as_deref(), Some("meta_dept"));

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: "meta_emp".into(),
        detail: Detail::Indexes,
    }) else {
        panic!("expected a table")
    };
    let index = rows
        .iter()
        .find(|r| r[0].as_deref() == Some("meta_emp_dept"))
        .expect("the index");
    assert_eq!(index[3].as_deref(), Some("deptno"));

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: "meta_dept".into(),
        detail: Detail::UsedBy,
    }) else {
        panic!("expected a table")
    };
    let used_by: Vec<_> = rows
        .iter()
        .map(|r| r[1].clone().unwrap_or_default())
        .collect();
    assert!(used_by.contains(&"meta_v".to_owned()), "{used_by:?}");
    assert!(used_by.contains(&"meta_emp".to_owned()), "{used_by:?}");

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: "meta_v".into(),
        detail: Detail::Uses,
    }) else {
        panic!("expected a table")
    };
    assert_eq!(rows.len(), 2, "{rows:?}");

    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: schema.clone(),
        name: "meta_emp".into(),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    assert!(
        ddl.starts_with(&format!(
            "create table {schema}.meta_emp (\n    empno integer generated always as identity not null,"
        )),
        "{ddl}"
    );
    assert!(ddl.contains("\"Salary\" numeric(8,2) default 0"), "{ddl}");
    assert!(
        ddl.contains(&format!(
            "constraint meta_emp_fk FOREIGN KEY (deptno) REFERENCES {schema}.meta_dept(deptno)"
        )),
        "{ddl}"
    );
    assert!(ddl.contains("CREATE INDEX meta_emp_dept ON"), "{ddl}");
    // The DDL runs: it rebuilds the tables elsewhere, the key referencing
    // the copy's department.
    h.ok("drop schema if exists meta_copy cascade");
    h.ok("create schema meta_copy");
    for table in ["meta_dept", "meta_emp"] {
        let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
            owner: schema.clone(),
            name: table.into(),
            object_type: ObjectType::Table,
        }) else {
            panic!("expected DDL")
        };
        for statement in pythia_plsql::split_statements_in(&ddl, pythia_plsql::Syntax::Postgres) {
            let sql = statement
                .text(&ddl)
                .replace(&format!("{schema}."), "meta_copy.");
            h.ok(&sql);
        }
    }
    h.ok("drop schema meta_copy cascade");

    let MetaResult::Text(view) = h.meta(MetaQuery::Ddl {
        owner: schema.clone(),
        name: "meta_v".into(),
        object_type: ObjectType::View,
    }) else {
        panic!("expected DDL")
    };
    assert!(
        view.starts_with(&format!("create or replace view {schema}.meta_v as\n")),
        "{view}"
    );
    assert!(view.ends_with(';'), "{view}");

    let MetaResult::Text(source) = h.meta(MetaQuery::Source {
        owner: schema.clone(),
        name: "meta_f".into(),
        object_type: ObjectType::Function,
    }) else {
        panic!("expected source")
    };
    assert!(source.starts_with("CREATE OR REPLACE FUNCTION"), "{source}");

    let MetaResult::Objects(found) = h.meta(MetaQuery::Find {
        pattern: "meta_*".into(),
        limit: 10,
    }) else {
        panic!("expected objects")
    };
    assert!(
        found
            .iter()
            .any(|o| o.name == "meta_f" && o.object_type == ObjectType::Function)
    );
    assert_eq!(found[0].owner, schema, "the current schema comes first");

    let MetaResult::Table { columns, rows } = h.meta(MetaQuery::ExplainPlan {
        sql: "select * from meta_emp e join meta_dept d on d.deptno = e.deptno \
              where e.empno = :id"
            .into(),
    }) else {
        panic!("expected a plan")
    };
    assert_eq!(columns[0].name, "ID");
    assert!(
        rows.iter()
            .any(|r| r[4].as_deref().is_some_and(|o| o.starts_with("meta_"))),
        "{rows:?}"
    );
    // An array slice's bounds are no binds: explained as written.
    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::ExplainPlan {
        sql: "select (array[1,2,3])[1:2] from meta_emp".into(),
    }) else {
        panic!("expected a plan")
    };
    assert!(!rows.is_empty());

    h.ok("drop view meta_v");
    h.ok("drop table meta_emp");
    h.ok("drop table meta_dept");
    h.ok("drop function meta_f(int)");
}

/// Tables of the same name in several schemas: a foreign key names the
/// table it references with its schema unless it's the table's own, in the
/// constraints and whatever the session's search path; the DDL names every
/// schema, so that it builds the table again elsewhere with its own
/// references.
#[test]
fn foreign_keys_name_their_schema() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists fk_same_one, fk_same_two, fk_same_copy cascade");
    h.ok("drop table if exists fk_same_users");
    h.ok("create table fk_same_users (id int primary key)");
    for tenant in ["fk_same_one", "fk_same_two", "fk_same_copy"] {
        h.ok(&format!("create schema {tenant}"));
        h.ok(&format!(
            "create table {tenant}.fk_same_users (id int primary key)"
        ));
    }
    h.ok(
        "create table fk_same_one.fk_same_note (id int primary key, \
         own int constraint fk_own references fk_same_one.fk_same_users, \
         shared int constraint fk_shared references public.fk_same_users)",
    );
    h.ok(
        "create table fk_same_two.fk_same_note (id int primary key, \
         other int constraint fk_other references fk_same_one.fk_same_users)",
    );
    let foreign_keys = |h: &mut Harness, owner: &str| -> Vec<[String; 3]> {
        let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
            owner: owner.into(),
            name: "fk_same_note".into(),
            detail: Detail::Constraints,
        }) else {
            panic!("expected a table")
        };
        rows.into_iter()
            .filter(|r| r[1].as_deref() == Some("Foreign key"))
            .map(|r| [0, 2, 3].map(|i| r[i].clone().unwrap_or_default()))
            .collect()
    };
    assert_eq!(
        foreign_keys(&mut h, "fk_same_one"),
        [
            [
                "fk_own",
                "FOREIGN KEY (own) REFERENCES fk_same_users(id)",
                "fk_same_users"
            ],
            [
                "fk_shared",
                "FOREIGN KEY (shared) REFERENCES public.fk_same_users(id)",
                "public.fk_same_users"
            ],
        ]
    );
    // The session's path finds fk_same_one's table first: the reference
    // from fk_same_two still says whose it is.
    h.ok("set search_path = fk_same_one, public");
    assert_eq!(
        foreign_keys(&mut h, "fk_same_two"),
        [[
            "fk_other",
            "FOREIGN KEY (other) REFERENCES fk_same_one.fk_same_users(id)",
            "fk_same_one.fk_same_users"
        ]]
    );
    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: "fk_same_one".into(),
        name: "fk_same_note".into(),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    for constraint in [
        "constraint fk_own FOREIGN KEY (own) REFERENCES fk_same_one.fk_same_users(id)",
        "constraint fk_shared FOREIGN KEY (shared) REFERENCES public.fk_same_users(id)",
    ] {
        assert!(ddl.contains(constraint), "{ddl}");
    }
    assert_eq!(
        h.scalar("select current_setting('search_path')").as_deref(),
        Some("fk_same_one, public"),
        "the session keeps its path"
    );
    // Run in another schema, the table references that schema's users.
    for statement in pythia_plsql::split_statements_in(&ddl, pythia_plsql::Syntax::Postgres) {
        h.ok(&statement
            .text(&ddl)
            .replace("fk_same_one.", "fk_same_copy."));
    }
    assert_eq!(
        h.scalar(
            "select confrelid::regclass::text from pg_constraint \
              where conname = 'fk_own' and conrelid = 'fk_same_copy.fk_same_note'::regclass"
        )
        .as_deref(),
        Some("fk_same_copy.fk_same_users")
    );
    h.ok("reset search_path");
    h.ok("drop schema fk_same_one, fk_same_two, fk_same_copy cascade");
    h.ok("drop table fk_same_users");
}

/// A table's indexes of every kind, each with its columns as its definition
/// has them (order, collation, operator class), INCLUDE columns and
/// predicate.
#[test]
fn indexes_as_defined() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    h.ok("drop table if exists idx_kinds");
    h.ok(
        "create table idx_kinds (id int primary key, name text, code text, n int, \
         tags text[], during tsrange, at timestamptz)",
    );
    for sql in [
        "create index idx_kinds_fts on idx_kinds using gin (to_tsvector('portuguese', name))",
        "create index idx_kinds_tags on idx_kinds using gin (tags)",
        "create index idx_kinds_during on idx_kinds using gist (during)",
        "create index idx_kinds_at on idx_kinds using brin (at)",
        "create index idx_kinds_lower on idx_kinds (lower(code) desc nulls last, n nulls first)",
        "create index idx_kinds_like on idx_kinds (code collate \"C\" text_pattern_ops)",
        "create unique index idx_kinds_cover on idx_kinds (code) include (name, n)",
        "create index idx_kinds_positive on idx_kinds (n desc) where n > 0",
    ] {
        h.ok(sql);
    }
    let MetaResult::Table { columns, rows } = h.meta(MetaQuery::Details {
        owner: schema,
        name: "idx_kinds".into(),
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
    let indexes: Vec<Vec<&str>> = rows
        .iter()
        .map(|r| r.iter().map(|v| v.as_deref().unwrap_or("")).collect())
        .collect();
    assert_eq!(
        indexes,
        [
            ["idx_kinds_pkey", "btree", "PRIMARY", "id", "", ""],
            ["idx_kinds_at", "brin", "NONUNIQUE", "at", "", ""],
            ["idx_kinds_cover", "btree", "UNIQUE", "code", "name, n", ""],
            ["idx_kinds_during", "gist", "NONUNIQUE", "during", "", ""],
            [
                "idx_kinds_fts",
                "gin",
                "NONUNIQUE",
                "to_tsvector('portuguese'::regconfig, name)",
                "",
                ""
            ],
            [
                "idx_kinds_like",
                "btree",
                "NONUNIQUE",
                "code COLLATE \"C\" text_pattern_ops",
                "",
                ""
            ],
            [
                "idx_kinds_lower",
                "btree",
                "NONUNIQUE",
                "lower(code) DESC NULLS LAST, n NULLS FIRST",
                "",
                ""
            ],
            [
                "idx_kinds_positive",
                "btree",
                "NONUNIQUE",
                "n DESC",
                "",
                "n > 0"
            ],
            ["idx_kinds_tags", "gin", "NONUNIQUE", "tags", "", ""],
        ]
    );
    h.ok("drop table idx_kinds");
}

/// Generated columns read as declared, PostgreSQL 18's virtual ones too: as
/// their default, and in DDL that builds the table again.
#[test]
fn generated_columns_as_declared() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let release: u32 = h
        .scalar("select current_setting('server_version_num')")
        .and_then(|v| v.parse().ok())
        .unwrap_or_default();
    let schema = h.schema.clone();
    h.ok("drop schema if exists gen_copy cascade");
    h.ok("drop table if exists gen_cols");
    let virtual_column = if release >= 180000 {
        ", twice int generated always as (a * 2) virtual"
    } else {
        ""
    };
    h.ok(&format!(
        "create table gen_cols (id int generated always as identity, a int, \
         thrice int generated always as (a * 3) stored{virtual_column})"
    ));
    let MetaResult::Columns(columns) = h.meta(MetaQuery::Columns {
        owner: schema.clone(),
        table: "gen_cols".into(),
        keys: false,
    }) else {
        panic!("expected columns")
    };
    let defaults: Vec<(&str, Option<&str>)> = columns
        .iter()
        .map(|c| (c.name.as_str(), c.default.as_deref()))
        .collect();
    let mut expected = vec![
        ("id", Some("generated always as identity")),
        ("a", None),
        ("thrice", Some("generated always as ((a * 3)) stored")),
    ];
    if release >= 180000 {
        expected.push(("twice", Some("generated always as ((a * 2)) virtual")));
    }
    assert_eq!(defaults, expected);
    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: schema.clone(),
        name: "gen_cols".into(),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    h.ok("create schema gen_copy");
    for statement in pythia_plsql::split_statements_in(&ddl, pythia_plsql::Syntax::Postgres) {
        h.ok(&statement
            .text(&ddl)
            .replace(&format!("{schema}."), "gen_copy."));
    }
    assert_eq!(
        h.scalar(
            "select string_agg(attname || ':' || attgenerated::text, ' ' order by attnum) \
               from pg_attribute where attrelid = 'gen_copy.gen_cols'::regclass \
                and attgenerated <> ''"
        )
        .as_deref(),
        Some(if release >= 180000 {
            "thrice:s twice:v"
        } else {
            "thrice:s"
        }),
        "{ddl}"
    );
    h.ok("drop schema gen_copy cascade");
    h.ok("drop table gen_cols");
}

/// A table's DDL has the sequences its columns own (serial columns) and
/// its identity columns' options: run in another schema, the copy numbers
/// its rows with sequences of its own, as the table does. A sequence the
/// table doesn't own stays the one it uses.
#[test]
fn owned_sequences_come_with_the_table() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r53_seq, r53_seq_copy cascade");
    h.ok("create schema r53_seq");
    h.ok("create schema r53_seq_copy");
    h.ok("create sequence r53_seq.r53_free start 500");
    h.ok(
        "create table r53_seq.r53_t (id serial primary key, big bigserial, \
         n int generated by default as identity (start with 10 increment by 5 cache 3), \
         a int generated always as identity, \
         down smallint generated by default as identity (increment by -1), \
         free int default nextval('r53_seq.r53_free'), name text)",
    );
    h.ok("alter sequence r53_seq.r53_t_big_seq increment by 2 start with 7 restart");
    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: "r53_seq".into(),
        name: "r53_t".into(),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    for line in [
        "n integer generated by default as identity (start with 10 increment by 5 cache 3) not null",
        "a integer generated always as identity not null",
        "down smallint generated by default as identity (increment by -1) not null",
        "alter sequence r53_seq.r53_t_id_seq owned by r53_seq.r53_t.id;",
    ] {
        assert!(ddl.contains(line), "{ddl}");
    }
    for statement in pythia_plsql::split_statements_in(&ddl, pythia_plsql::Syntax::Postgres) {
        let sql = statement
            .text(&ddl)
            .replace("r53_seq.r53_t", "r53_seq_copy.r53_t");
        h.execute(0, &sql, 100);
        if let DbEvent::Failed { error, .. } = h.next_result() {
            panic!("{sql}: {error}\n\n{ddl}");
        }
        h.settle();
    }
    h.ok("insert into r53_seq_copy.r53_t (name) values ('x'), ('y')");
    assert_eq!(
        h.scalar(
            "select string_agg(concat_ws(' ', id, big, n, a, down, free), ', ' order by id) \
               from r53_seq_copy.r53_t"
        )
        .as_deref(),
        Some("1 7 10 1 -1 500, 2 9 15 2 -2 501"),
        "{ddl}"
    );
    // The copy's sequences are its own, owned by its columns.
    assert_eq!(
        h.scalar(
            "select string_agg(c || ' ' \
                               || coalesce(pg_get_serial_sequence('r53_seq_copy.r53_t', c), '-'), \
                               ', ' order by c) \
               from unnest(array['id', 'big', 'n', 'a', 'free']) c"
        )
        .as_deref(),
        Some(
            "a r53_seq_copy.r53_t_a_seq, big r53_seq_copy.r53_t_big_seq, \
             free -, id r53_seq_copy.r53_t_id_seq, n r53_seq_copy.r53_t_n_seq"
        ),
        "{ddl}"
    );
    assert_eq!(
        h.scalar("select is_called::text from r53_seq.r53_t_id_seq")
            .as_deref(),
        Some("false"),
        "the table's own sequence isn't used"
    );
    h.ok("drop schema r53_seq, r53_seq_copy cascade");
}

/// Runs the statements of `ddl` one by one, `from` replaced by `to` in each.
fn run_ddl(h: &mut Harness, ddl: &str, from: &str, to: &str) {
    for statement in pythia_plsql::split_statements_in(ddl, pythia_plsql::Syntax::Postgres) {
        let sql = statement.text(ddl).replace(from, to);
        h.execute(0, &sql, 100);
        if let DbEvent::Failed { error, .. } = h.next_result() {
            panic!("{sql}: {error}\n\n{ddl}");
        }
        h.settle();
    }
}

/// A sequence a column owns names that column in its DDL (#307): run
/// elsewhere, the copy goes with the copy's column, as the original does.
#[test]
fn a_sequence_names_the_column_that_owns_it() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r62_seq, r62_seq_copy cascade");
    h.ok("create schema r62_seq");
    h.ok("create schema r62_seq_copy");
    h.ok("create table r62_seq.r62_t (id serial, \"N\" int)");
    h.ok("create sequence r62_seq.r62_n start 100 owned by r62_seq.r62_t.\"N\"");
    h.ok("create sequence r62_seq.r62_free");
    let ddl = |h: &mut Harness, name: &str| {
        let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
            owner: "r62_seq".into(),
            name: name.into(),
            object_type: ObjectType::Sequence,
        }) else {
            panic!("expected DDL")
        };
        ddl
    };
    let id = ddl(&mut h, "r62_t_id_seq");
    assert!(id.ends_with("\n  owned by r62_seq.r62_t.id;"), "{id}");
    let n = ddl(&mut h, "r62_n");
    assert!(n.ends_with("\n  owned by r62_seq.r62_t.\"N\";"), "{n}");
    let free = ddl(&mut h, "r62_free");
    assert!(!free.contains("owned by"), "{free}");
    h.ok("create table r62_seq_copy.r62_t (id int, \"N\" int)");
    for ddl in [&id, &n, &free] {
        run_ddl(&mut h, ddl, "r62_seq.", "r62_seq_copy.");
    }
    assert_eq!(
        h.scalar(
            "select pg_get_serial_sequence('r62_seq_copy.r62_t', 'id') || ' ' \
                    || pg_get_serial_sequence('r62_seq_copy.r62_t', 'N')"
        )
        .as_deref(),
        Some("r62_seq_copy.r62_t_id_seq r62_seq_copy.r62_n")
    );
    // They go with the copy's table.
    h.ok("drop table r62_seq_copy.r62_t");
    assert_eq!(
        h.scalar(
            "select string_agg(relname, ' ') from pg_class \
              where relnamespace = 'r62_seq_copy'::regnamespace"
        )
        .as_deref(),
        Some("r62_free")
    );
    h.ok("drop schema r62_seq, r62_seq_copy cascade");
}

/// An identity column's sequence name is in the table's DDL where it isn't
/// the one PostgreSQL chooses (#308), which is left out so that a copy
/// chooses its own.
#[test]
fn identities_keep_their_sequence_names() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r62_id, r62_id_copy cascade");
    h.ok("create schema r62_id");
    h.ok("create schema r62_id_copy");
    h.ok("create table r62_id.r62_t ( \
           a int generated by default as identity (sequence name r62_id.r62_a_ids start with 10), \
           b int generated always as identity, \
           c int generated always as identity (sequence name \"R62 Odd\" increment by 2), \
           name text)");
    let long = "r62_a_rather_long_table_name_for_its_identities";
    h.ok(&format!(
        "create table r62_id.{long} (a_rather_long_column_name_too int generated by default as identity)"
    ));
    let ddl = |h: &mut Harness, name: &str| {
        let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
            owner: "r62_id".into(),
            name: name.into(),
            object_type: ObjectType::Table,
        }) else {
            panic!("expected DDL")
        };
        ddl
    };
    let t = ddl(&mut h, "r62_t");
    for line in [
        "a integer generated by default as identity \
         (sequence name r62_id.r62_a_ids start with 10) not null",
        "b integer generated always as identity not null",
        "c integer generated always as identity \
         (sequence name r62_id.\"R62 Odd\" increment by 2) not null",
    ] {
        assert!(t.contains(line), "{t}");
    }
    let MetaResult::Columns(columns) = h.meta(MetaQuery::Columns {
        owner: "r62_id".into(),
        table: "r62_t".into(),
        keys: false,
    }) else {
        panic!("expected columns")
    };
    assert_eq!(
        columns[0].default.as_deref(),
        Some("generated by default as identity (sequence name r62_id.r62_a_ids start with 10)")
    );
    // PostgreSQL's own choice, cut to 63 bytes, at a whole letter.
    let l = ddl(&mut h, long);
    assert!(
        l.contains(
            "a_rather_long_column_name_too integer generated by default as identity not null"
        ),
        "{l}"
    );
    let czech = "čajový_stůl_s_dlouhým_názvem_tabulky_x";
    h.ok(&format!(
        "create table r62_id.\"{czech}\" (\"sloupec_č_dlouhý_název_sloupce_y\" int \
         generated by default as identity)"
    ));
    let c = ddl(&mut h, czech);
    assert!(
        c.contains(
            "\"sloupec_č_dlouhý_název_sloupce_y\" integer generated by default as identity not null"
        ),
        "{c}"
    );
    run_ddl(&mut h, &t, "r62_id.", "r62_id_copy.");
    assert_eq!(
        h.scalar(
            "select string_agg(c || ' ' || pg_get_serial_sequence('r62_id_copy.r62_t', c), ', ' \
                               order by c) \
               from unnest(array['a', 'b', 'c']) c"
        )
        .as_deref(),
        Some(
            "a r62_id_copy.r62_a_ids, b r62_id_copy.r62_t_b_seq, \
             c r62_id_copy.\"R62 Odd\""
        ),
        "{t}"
    );
    // A copy beside it chooses a sequence of its own.
    run_ddl(&mut h, &l, long, &format!("{long}_2"));
    h.ok("drop schema r62_id, r62_id_copy cascade");
}

fn ddl_of(h: &mut Harness, owner: &str, name: &str, object_type: ObjectType) -> String {
    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: owner.into(),
        name: name.into(),
        object_type,
    }) else {
        panic!("expected DDL")
    };
    ddl
}

/// A foreign key to the table's own unique index (one no constraint
/// makes) comes after that index in the DDL, which kept the index out
/// and put the key in the create: run, it failed with "there is no unique
/// constraint matching given keys".
#[test]
fn a_key_to_the_tables_own_unique_index_runs_again() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r95_tree, r95_tree_copy cascade");
    h.ok("create schema r95_tree");
    h.ok("create schema r95_tree_copy");
    h.ok(
        "create table r95_tree.tree (id int primary key, code text not null, \
           parent_code text, parent_id int constraint tree_up references r95_tree.tree)",
    );
    h.ok("create unique index tree_code on r95_tree.tree (code)");
    h.ok(
        "alter table r95_tree.tree add constraint tree_parent foreign key (parent_code) \
           references r95_tree.tree (code)",
    );
    let ddl = ddl_of(&mut h, "r95_tree", "tree", ObjectType::Table);
    assert!(ddl.contains("CREATE UNIQUE INDEX tree_code"), "{ddl}");
    let index = ddl.find("CREATE UNIQUE INDEX tree_code").unwrap();
    let key = ddl.find("constraint tree_parent").expect(&ddl);
    assert!(index < key, "{ddl}");
    // A key to the primary key stays in the create.
    assert!(ddl.find("constraint tree_up").unwrap() < index, "{ddl}");
    run_ddl(&mut h, &ddl, "r95_tree.", "r95_tree_copy.");
    h.ok("insert into r95_tree_copy.tree values (1, 'a', null, null), (2, 'b', 'a', 1)");
    h.ok("drop schema r95_tree, r95_tree_copy cascade");
}

/// A foreign table's DDL makes a foreign table again, on its server with
/// its options and its columns', not a plain table: run elsewhere, it made
/// an empty local one. Needs postgres_fdw, which a superuser can create.
#[test]
fn a_foreign_tables_ddl_makes_a_foreign_table() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let installed = h
        .scalar("select count(*) from pg_extension where extname = 'postgres_fdw'")
        .as_deref()
        == Some("1");
    let superuser = h
        .scalar("select rolsuper from pg_roles where rolname = current_user")
        .as_deref()
        == Some("t");
    if !installed && !superuser {
        return;
    }
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("create extension if not exists postgres_fdw");
    h.ok("drop schema if exists r96_ft, r96_ft_copy cascade");
    h.ok("drop server if exists r96_srv cascade");
    h.ok("create server r96_srv foreign data wrapper postgres_fdw options (dbname 'pythia')");
    h.ok("create schema r96_ft");
    h.ok("create schema r96_ft_copy");
    h.ok(
        "create foreign table r96_ft.remote (id int options (column_name 'ident') not null, \
           name text) server r96_srv options (schema_name 'public', table_name 'it''s')",
    );
    h.ok("comment on foreign table r96_ft.remote is 'over there'");
    let ddl = ddl_of(&mut h, "r96_ft", "remote", ObjectType::Table);
    assert!(
        ddl.starts_with("create foreign table r96_ft.remote ("),
        "{ddl}"
    );
    assert!(
        ddl.contains("id integer options (column_name 'ident') not null"),
        "{ddl}"
    );
    assert!(
        ddl.contains(") server r96_srv options (schema_name 'public', table_name 'it''s');"),
        "{ddl}"
    );
    run_ddl(&mut h, &ddl, "r96_ft.", "r96_ft_copy.");
    assert_eq!(
        h.scalar(
            "select c.relkind::text from pg_class c join pg_namespace n on n.oid = c.relnamespace \
              where n.nspname = 'r96_ft_copy' and c.relname = 'remote'"
        )
        .as_deref(),
        Some("f")
    );
    h.ok("drop schema r96_ft, r96_ft_copy cascade");
    h.ok("drop server r96_srv cascade");
}

/// A view's DDL names its tables and functions with their schemas, as a
/// table's does (#226): run with another search path, by Edit in a window
/// that set one, the view reads the same tables as before (#329).
#[test]
fn a_views_ddl_names_its_tables_with_their_schemas() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    h.ok("drop schema if exists r73_vp, r73_vp_other cascade");
    h.ok("drop table if exists r73_vp_t");
    h.ok("drop function if exists r73_vp_f(int)");
    h.ok("create schema r73_vp");
    h.ok("create schema r73_vp_other");
    for (owner, n) in [(schema.as_str(), 1), ("r73_vp_other", 2)] {
        h.ok(&format!("create table {owner}.r73_vp_t (n int)"));
        h.ok(&format!("insert into {owner}.r73_vp_t values ({n})"));
        h.ok(&format!(
            "create function {owner}.r73_vp_f(x int) returns int language sql \
             as 'select x * {n}'"
        ));
    }
    // Names the session's path finds.
    h.ok("create view r73_vp.r73_v as select r73_vp_f(n) as n from r73_vp_t");
    h.ok("create materialized view r73_vp.r73_mv as select r73_vp_f(n) as n from r73_vp_t");
    let view = ddl_of(&mut h, "r73_vp", "r73_v", ObjectType::View);
    let materialized = ddl_of(&mut h, "r73_vp", "r73_mv", ObjectType::MaterializedView);
    for ddl in [&view, &materialized] {
        assert!(
            ddl.contains(&format!(
                "SELECT {schema}.r73_vp_f(n) AS n\n   FROM {schema}.r73_vp_t"
            )),
            "{ddl}"
        );
    }
    h.ok("set search_path = r73_vp_other, public");
    run_ddl(&mut h, &view, "", "");
    h.ok("drop materialized view r73_vp.r73_mv");
    run_ddl(&mut h, &materialized, "", "");
    h.ok("reset search_path");
    assert_eq!(
        h.scalar("select (select n from r73_vp.r73_v) || ' ' || (select n from r73_vp.r73_mv)")
            .as_deref(),
        Some("1 1"),
        "{view}\n\n{materialized}"
    );
    h.ok("drop schema r73_vp, r73_vp_other cascade");
    h.ok("drop table r73_vp_t");
    h.ok("drop function r73_vp_f(int)");
}

/// The DDL of a trigger and of a routine names its tables, functions and
/// types with their schemas, as a view's does (#329): run with another
/// search path, by Edit in a window that set one, the trigger stays on its
/// table and calls its function, and each routine is replaced rather than
/// made again with other argument types (#350).
/// A trigger's DDL keeps a trigger that isn't enabled as created so:
/// disabled, replica or always. Created again, it fired on every insert.
#[test]
fn a_triggers_ddl_keeps_its_state() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r98_tr cascade");
    h.ok("create schema r98_tr");
    for sql in [
        "create table r98_tr.orders (n int)",
        "create table r98_tr.items (n int)",
        "create table r98_tr.lines (n int)",
        "create function r98_tr.audit() returns trigger language plpgsql \
           as 'begin new.n = -1; return new; end'",
        "create trigger audit before insert on r98_tr.orders for each row \
           execute function r98_tr.audit()",
        "create trigger audit before insert on r98_tr.items for each row \
           execute function r98_tr.audit()",
        "create trigger audit before insert on r98_tr.lines for each row \
           execute function r98_tr.audit()",
        "alter table r98_tr.items disable trigger audit",
        "alter table r98_tr.lines enable always trigger audit",
    ] {
        h.ok(sql);
    }
    let ddl = ddl_of(&mut h, "r98_tr", "audit", ObjectType::Trigger);
    assert!(
        ddl.contains("alter table r98_tr.items disable trigger audit;"),
        "{ddl}"
    );
    assert!(
        ddl.contains("alter table r98_tr.lines enable always trigger audit;"),
        "{ddl}"
    );
    assert!(!ddl.contains("r98_tr.orders disable"), "{ddl}");
    // Run again as Edit runs it, each trigger replaced: the states stay.
    let script = ddl.replace("CREATE TRIGGER", "CREATE OR REPLACE TRIGGER");
    run_ddl(&mut h, &script, "", "");
    let states = h.scalar(
        "select string_agg(c.relname || '=' || t.tgenabled::text, ',' order by c.relname) \
           from pg_trigger t join pg_class c on c.oid = t.tgrelid \
          where t.tgname = 'audit' and c.relnamespace = 'r98_tr'::regnamespace",
    );
    assert_eq!(states.as_deref(), Some("items=D,lines=A,orders=O"));
    h.ok("drop schema r98_tr cascade");
}

#[test]
fn the_ddl_of_triggers_and_routines_names_their_schemas() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    h.ok("drop schema if exists r81_sp, r81_sp_other cascade");
    h.ok("drop table if exists r81_sp_t");
    h.ok("drop function if exists r81_sp_tf(), r81_sp_g(int)");
    h.ok("drop domain if exists r81_sp_d");
    h.ok("create schema r81_sp");
    h.ok("create schema r81_sp_other");
    for (owner, n) in [(schema.as_str(), 1), ("r81_sp_other", 2)] {
        h.ok(&format!("create table {owner}.r81_sp_t (n int)"));
        h.ok(&format!("create domain {owner}.r81_sp_d as int"));
        h.ok(&format!(
            "create function {owner}.r81_sp_g(x int) returns int language sql \
             as 'select x * {n}'"
        ));
        h.ok(&format!(
            "create function {owner}.r81_sp_tf() returns trigger language plpgsql \
             as 'begin return new; end'"
        ));
    }
    // Names the session's path finds.
    h.ok(
        "create trigger r81_sp_tr before insert on r81_sp_t for each row \
          when (r81_sp_g(new.n) > 0) execute function r81_sp_tf()",
    );
    h.ok(
        "create function r81_sp.r81_sp_f(x r81_sp_d) returns r81_sp_d language sql \
          return r81_sp_g(x)",
    );
    h.ok("revoke execute on function r81_sp.r81_sp_f(r81_sp_d) from public");
    h.ok("create procedure r81_sp.r81_sp_p(x r81_sp_d) language sql \
          begin atomic insert into r81_sp_t values (r81_sp_g(x)); end");
    let trigger = ddl_of(&mut h, &schema, "r81_sp_tr", ObjectType::Trigger);
    assert_eq!(
        trigger,
        format!(
            "CREATE TRIGGER r81_sp_tr BEFORE INSERT ON {schema}.r81_sp_t FOR EACH ROW \
             WHEN ({schema}.r81_sp_g(new.n) > 0) EXECUTE FUNCTION {schema}.r81_sp_tf();"
        )
    );
    let function = ddl_of(&mut h, "r81_sp", "r81_sp_f", ObjectType::Function);
    for part in [
        format!("r81_sp.r81_sp_f(x {schema}.r81_sp_d)\n RETURNS {schema}.r81_sp_d\n"),
        format!("RETURN {schema}.r81_sp_g("),
        format!("revoke execute on function r81_sp.r81_sp_f(x {schema}.r81_sp_d) from public;"),
    ] {
        assert!(function.contains(&part), "{function}");
    }
    let procedure = ddl_of(&mut h, "r81_sp", "r81_sp_p", ObjectType::Procedure);
    for part in [
        format!("r81_sp.r81_sp_p(IN x {schema}.r81_sp_d)\n"),
        format!("INSERT INTO {schema}.r81_sp_t"),
        format!("{schema}.r81_sp_g("),
    ] {
        assert!(procedure.contains(&part), "{procedure}");
    }
    // As Edit runs them.
    h.ok(&format!("set search_path = r81_sp_other, {schema}"));
    run_ddl(
        &mut h,
        &trigger.replacen("CREATE TRIGGER", "CREATE OR REPLACE TRIGGER", 1),
        "",
        "",
    );
    run_ddl(&mut h, &function, "", "");
    run_ddl(&mut h, &procedure, "", "");
    h.ok("reset search_path");
    assert_eq!(
        h.scalar(
            "select string_agg(c.relnamespace::regnamespace || '.' || c.relname || ' ' \
                               || p.pronamespace::regnamespace || '.' || p.proname, ', ') \
               from pg_trigger t join pg_class c on c.oid = t.tgrelid \
               join pg_proc p on p.oid = t.tgfoid \
              where t.tgname = 'r81_sp_tr'"
        ),
        Some(format!("{schema}.r81_sp_t {schema}.r81_sp_tf")),
        "{trigger}"
    );
    assert_eq!(
        h.scalar(
            "select string_agg(p.proname || '(' || t.typnamespace::regnamespace || ')', ', ' \
                               order by p.proname, t.typnamespace::regnamespace::text) \
               from pg_proc p join pg_type t on t.oid = p.proargtypes[0] \
              where p.pronamespace = 'r81_sp'::regnamespace"
        ),
        Some(format!("r81_sp_f({schema}), r81_sp_p({schema})")),
        "{function}\n\n{procedure}"
    );
    assert_eq!(h.scalar("select r81_sp.r81_sp_f(3)").as_deref(), Some("3"));
    h.ok("drop schema r81_sp, r81_sp_other cascade");
    h.ok("drop table r81_sp_t");
    h.ok("drop function r81_sp_tf(), r81_sp_g(int)");
    h.ok("drop domain r81_sp_d");
}

/// A view's DDL keeps its security_barrier, security_invoker and check
/// option, which pg_get_viewdef leaves out: Edit's CREATE OR REPLACE
/// without them reset them (#351). Run in place, as Edit runs it, and
/// in another schema, the views have the options they had.
#[test]
fn a_views_ddl_keeps_its_security_and_check_option() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r81_vo, r81_vo_copy cascade");
    h.ok("create schema r81_vo");
    h.ok("create schema r81_vo_copy");
    for schema in ["r81_vo", "r81_vo_copy"] {
        h.ok(&format!("create table {schema}.r81_t (n int)"));
    }
    h.ok("create view r81_vo.r81_barrier with (security_barrier) as \
          select n from r81_vo.r81_t where n > 0 with local check option");
    h.ok(
        "create view r81_vo.r81_invoker with (security_invoker = true, security_barrier = false) \
          as select n from r81_vo.r81_barrier with cascaded check option",
    );
    h.ok("create view r81_vo.r81_plain as select n from r81_vo.r81_t");
    let barrier = ddl_of(&mut h, "r81_vo", "r81_barrier", ObjectType::View);
    assert_eq!(
        barrier,
        "create or replace view r81_vo.r81_barrier with (security_barrier='true') as\n \
         SELECT n\n   FROM r81_vo.r81_t\n  WHERE n > 0\nwith local check option;"
    );
    let invoker = ddl_of(&mut h, "r81_vo", "r81_invoker", ObjectType::View);
    assert!(
        invoker.starts_with(
            "create or replace view r81_vo.r81_invoker \
             with (security_invoker='true', security_barrier='false') as\n"
        ),
        "{invoker}"
    );
    assert!(
        invoker.ends_with("\nwith cascaded check option;"),
        "{invoker}"
    );
    let plain = ddl_of(&mut h, "r81_vo", "r81_plain", ObjectType::View);
    assert_eq!(
        plain,
        "create or replace view r81_vo.r81_plain as\n SELECT n\n   FROM r81_vo.r81_t;"
    );
    let options = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "select string_agg(relname || ' ' || coalesce(array_to_string(reloptions, ','), '-'), \
                               '; ' order by relname) \
               from pg_class where relnamespace = '{schema}'::regnamespace and relkind = 'v'"
        ))
    };
    let was = options(&mut h, "r81_vo");
    assert_eq!(
        was.as_deref(),
        Some(
            "r81_barrier security_barrier=true,check_option=local; \
             r81_invoker security_invoker=true,security_barrier=false,check_option=cascaded; \
             r81_plain -"
        )
    );
    for ddl in [&barrier, &invoker, &plain] {
        run_ddl(&mut h, ddl, "", "");
    }
    assert_eq!(options(&mut h, "r81_vo"), was, "{barrier}\n\n{invoker}");
    for ddl in [&barrier, &invoker, &plain] {
        run_ddl(&mut h, ddl, "r81_vo.", "r81_vo_copy.");
    }
    assert_eq!(
        options(&mut h, "r81_vo_copy"),
        was,
        "{barrier}\n\n{invoker}"
    );
    h.fails("insert into r81_vo_copy.r81_barrier values (-1)");
    h.ok("drop schema r81_vo, r81_vo_copy cascade");
}

/// A table's DDL makes it as it is, as a materialized view's does (#330):
/// unlogged where it is, its serial sequence too, with its access method,
/// storage parameters (its TOAST table's too) and tablespace where they
/// aren't the defaults, and each index in its tablespace, a constraint's
/// with its storage parameters. Run in another schema, the copy is the
/// same (#352).
#[test]
fn a_tables_ddl_keeps_its_storage() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let superuser = h.scalar("select rolsuper::text from pg_roles where rolname = current_user");
    if superuser.as_deref() != Some("true") {
        eprintln!("not a superuser, skipping");
        return;
    }
    h.ok("drop schema if exists r81_st, r81_st_copy cascade");
    h.ok("drop tablespace if exists r81_space");
    h.ok("drop access method if exists r81_heap");
    h.ok("create schema r81_st");
    h.ok("create schema r81_st_copy");
    // A tablespace in the data directory, as tests make them.
    h.ok("set allow_in_place_tablespaces = on");
    h.ok("create tablespace r81_space location ''");
    h.ok("reset allow_in_place_tablespaces");
    h.ok("create access method r81_heap type table handler heap_tableam_handler");
    h.ok(
        "create unlogged table r81_st.r81_t (id serial, n int, s text, \
         constraint r81_t_pk primary key (id) with (fillfactor = 80) \
           using index tablespace r81_space, \
         constraint r81_t_n unique (n) using index tablespace r81_space deferrable, \
         constraint r81_t_x exclude (s with =) with (fillfactor = 50) \
           using index tablespace r81_space where (n > 0)) \
         using r81_heap with (fillfactor = 70, toast.autovacuum_enabled = false) \
         tablespace r81_space",
    );
    h.ok(
        "create index r81_t_s on r81_st.r81_t (s) with (fillfactor = 60) \
          tablespace r81_space where n > 0",
    );
    h.ok("create index r81_t_ns on r81_st.r81_t (n, s) tablespace r81_space");
    h.ok("create table r81_st.r81_plain (n int primary key, s text)");
    h.ok("create index r81_plain_s on r81_st.r81_plain (s)");
    h.ok(
        "create table r81_st.r81_parts (n int, primary key (n) with (fillfactor = 90)) \
          partition by range (n) tablespace r81_space",
    );
    let t = ddl_of(&mut h, "r81_st", "r81_t", ObjectType::Table);
    for part in [
        "create unlogged sequence r81_st.r81_t_id_seq as integer\n",
        "create unlogged table r81_st.r81_t (\n",
        "constraint r81_t_pk PRIMARY KEY (id) WITH (fillfactor='80') \
         USING INDEX TABLESPACE r81_space,\n",
        "constraint r81_t_n UNIQUE (n) USING INDEX TABLESPACE r81_space DEFERRABLE,\n",
        "constraint r81_t_x EXCLUDE USING btree (s WITH =) WITH (fillfactor='50') \
         USING INDEX TABLESPACE r81_space WHERE (n > 0)\n",
        "\n) using r81_heap with (fillfactor='70', toast.autovacuum_enabled='false') \
         tablespace r81_space;",
        "CREATE INDEX r81_t_s ON r81_st.r81_t USING btree (s) WITH (fillfactor='60') \
         TABLESPACE r81_space WHERE (n > 0);",
        "CREATE INDEX r81_t_ns ON r81_st.r81_t USING btree (n, s) TABLESPACE r81_space;",
    ] {
        assert!(t.contains(part), "{t}");
    }
    let plain = ddl_of(&mut h, "r81_st", "r81_plain", ObjectType::Table);
    for default in [
        "unlogged",
        " using ",
        " with (",
        "WITH (",
        "tablespace",
        "TABLESPACE",
    ] {
        assert!(!plain.contains(default), "{plain}");
    }
    let parts = ddl_of(&mut h, "r81_st", "r81_parts", ObjectType::Table);
    for part in [
        "constraint r81_parts_pkey PRIMARY KEY (n) WITH (fillfactor='90')\n",
        "\n) partition by RANGE (n) tablespace r81_space;",
    ] {
        assert!(parts.contains(part), "{parts}");
    }
    for ddl in [&t, &plain, &parts] {
        run_ddl(&mut h, ddl, "r81_st.", "r81_st_copy.");
    }
    let state = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', c.relname, c.relkind, c.relpersistence, c.reloptions, \
                                         s.reloptions, coalesce(t.spcname, '-'), \
                                         coalesce(a.amname, '-')), \
                               '; ' order by c.relname) \
               from pg_class c left join pg_am a on a.oid = c.relam \
               left join pg_class s on s.oid = c.reltoastrelid \
               left join pg_tablespace t on t.oid = c.reltablespace \
              where c.relnamespace = '{schema}'::regnamespace"
        ))
    };
    let was = state(&mut h, "r81_st");
    for relation in [
        "r81_t r u {fillfactor=70} {autovacuum_enabled=false} r81_space r81_heap",
        "r81_t_id_seq S u - -",
        "r81_t_pk i u {fillfactor=80} r81_space btree",
        "r81_t_s i u {fillfactor=60} r81_space btree",
        "r81_parts p p r81_space -",
        "r81_parts_pkey I p {fillfactor=90} - btree",
    ] {
        assert!(
            was.as_deref().is_some_and(|w| w.contains(relation)),
            "{was:?}"
        );
    }
    assert_eq!(state(&mut h, "r81_st_copy"), was, "{t}\n\n{parts}");
    h.ok("drop schema r81_st, r81_st_copy cascade");
    h.ok("drop tablespace r81_space");
    h.ok("drop access method r81_heap");
}

/// The DDL of a table, a view, a sequence and a routine gives it its owner
/// where that isn't the role reading it, as a materialized view's does
/// (#330): run in another schema, the copies have the same owners, a
/// table's serial sequence too, and the privileges an owner gave up stay
/// revoked (#353). Those of the role reading it have none.
#[test]
fn the_ddl_keeps_the_owner() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let superuser = h.scalar("select rolsuper::text from pg_roles where rolname = current_user");
    if superuser.as_deref() != Some("true") {
        eprintln!("not a superuser, skipping");
        return;
    }
    h.ok("drop schema if exists r81_ow, r81_ow_copy cascade");
    h.ok("drop role if exists \"R81 Owner\"");
    h.ok("create role \"R81 Owner\"");
    h.ok("create schema r81_ow");
    h.ok("create schema r81_ow_copy");
    for sql in [
        "create table r81_ow.r81_t (id serial, n int)",
        "alter table r81_ow.r81_t owner to \"R81 Owner\"",
        "revoke truncate on r81_ow.r81_t from \"R81 Owner\"",
        // A sequence a column owns has the table's owner.
        "create sequence r81_ow.r81_n",
        "alter sequence r81_ow.r81_n owner to \"R81 Owner\"",
        "alter sequence r81_ow.r81_n owned by r81_ow.r81_t.n",
        "create sequence r81_ow.r81_free",
        "alter sequence r81_ow.r81_free owner to \"R81 Owner\"",
        "create view r81_ow.r81_v as select n from r81_ow.r81_t",
        "alter view r81_ow.r81_v owner to \"R81 Owner\"",
        "create function r81_ow.r81_f(x int) returns int language sql as 'select x'",
        "alter function r81_ow.r81_f(int) owner to \"R81 Owner\"",
        "create function r81_ow.r81_f(x text) returns text language sql as 'select x'",
        "create procedure r81_ow.r81_p() language sql as 'select 1'",
        "alter procedure r81_ow.r81_p() owner to \"R81 Owner\"",
        "create view r81_ow.r81_mine as select 1 as one",
    ] {
        h.ok(sql);
    }
    let table = ddl_of(&mut h, "r81_ow", "r81_t", ObjectType::Table);
    let owned = table.find("alter sequence r81_ow.r81_n owned by r81_ow.r81_t.n;");
    let owner = table.find("\n\nalter table r81_ow.r81_t owner to \"R81 Owner\";\n\n");
    let revoke = table.find("revoke truncate on r81_ow.r81_t from \"R81 Owner\";");
    assert!(
        owned.is_some() && owned < owner && owner < revoke,
        "{table}"
    );
    let function = ddl_of(&mut h, "r81_ow", "r81_f", ObjectType::Function);
    assert!(
        function.contains(
            "$function$;\n\nalter function r81_ow.r81_f(x integer) owner to \"R81 Owner\";"
        ),
        "{function}"
    );
    assert_eq!(function.matches("owner to").count(), 1, "{function}");
    let n = ddl_of(&mut h, "r81_ow", "r81_n", ObjectType::Sequence);
    assert!(
        n.ends_with(
            "\n  start with 1 cache 1;\n\n\
             alter sequence r81_ow.r81_n owner to \"R81 Owner\";\n\n\
             alter sequence r81_ow.r81_n owned by r81_ow.r81_t.n;"
        ),
        "{n}"
    );
    let mine = ddl_of(&mut h, "r81_ow", "r81_mine", ObjectType::View);
    assert!(!mine.contains("owner to"), "{mine}");
    let mut ddls = vec![table];
    for (name, object_type) in [
        ("r81_free", ObjectType::Sequence),
        ("r81_v", ObjectType::View),
        ("r81_f", ObjectType::Function),
        ("r81_p", ObjectType::Procedure),
        ("r81_mine", ObjectType::View),
    ] {
        ddls.push(ddl_of(&mut h, "r81_ow", name, object_type));
    }
    for ddl in &ddls {
        run_ddl(&mut h, ddl, "r81_ow.", "r81_ow_copy.");
    }
    // Each object's owner and privileges.
    let owners = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "select string_agg(name || ' ' || owner || ' ' || coalesce(acl, '-'), '; ' \
                               order by name) \
               from (select relname as name, relowner::regrole::text as owner, \
                            relacl::text as acl \
                       from pg_class where relnamespace = '{schema}'::regnamespace \
                     union all \
                     select oid::regprocedure::text, proowner::regrole::text, proacl::text \
                       from pg_proc where pronamespace = '{schema}'::regnamespace) o"
        ))
        .map(|o| o.replace(&format!("{schema}."), ""))
    };
    let me = h
        .scalar("select current_user::regrole::text")
        .unwrap_or_default();
    let was = owners(&mut h, "r81_ow");
    assert!(
        was.as_deref().is_some_and(|w| {
            w.contains("r81_t_id_seq \"R81 Owner\"")
                && w.contains("r81_f(text) ")
                && w.contains(&format!("r81_mine {me} -"))
        }),
        "{was:?}"
    );
    assert_eq!(owners(&mut h, "r81_ow_copy"), was, "{}", ddls[0]);
    // The sequence a column owns, alone beside its table.
    h.ok("drop sequence r81_ow_copy.r81_n");
    run_ddl(&mut h, &n, "r81_ow.", "r81_ow_copy.");
    assert_eq!(owners(&mut h, "r81_ow_copy"), was, "{n}");
    h.ok("drop schema r81_ow, r81_ow_copy cascade");
    h.ok("drop role \"R81 Owner\"");
}

/// The DDL of a view, a sequence, a routine and a trigger has their
/// comments, a view's columns' too, as a table's and a materialized view's
/// has (#301), and a table's has its serial sequences': run in another
/// schema, the copies have them too (#354).
#[test]
fn the_ddl_has_the_comments() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r81_cm, r81_cm_copy cascade");
    h.ok("create schema r81_cm");
    h.ok("create schema r81_cm_copy");
    for sql in [
        "create table r81_cm.r81_t (id serial, n int)",
        "comment on sequence r81_cm.r81_t_id_seq is 'The ids'",
        "create view r81_cm.r81_v as select n from r81_cm.r81_t",
        "comment on view r81_cm.r81_v is 'It''s the view'",
        "comment on column r81_cm.r81_v.n is 'The n'",
        "create sequence r81_cm.r81_s",
        "comment on sequence r81_cm.r81_s is 'A sequence'",
        "create function r81_cm.r81_f(x int) returns int language sql as 'select x'",
        "comment on function r81_cm.r81_f(int) is 'For ints'",
        "create function r81_cm.r81_f(x text) returns text language sql as 'select x'",
        "create procedure r81_cm.r81_p() language sql as 'select 1'",
        "comment on procedure r81_cm.r81_p() is 'A procedure'",
        "create function r81_cm.r81_tf() returns trigger language plpgsql \
         as 'begin return new; end'",
        "create trigger r81_tr before insert on r81_cm.r81_t for each row \
         execute function r81_cm.r81_tf()",
        "comment on trigger r81_tr on r81_cm.r81_t is 'The trigger'",
    ] {
        h.ok(sql);
    }
    let mut ddls = Vec::new();
    for (name, object_type, comment) in [
        (
            "r81_t",
            ObjectType::Table,
            "\n\ncomment on sequence r81_cm.r81_t_id_seq is 'The ids';",
        ),
        (
            "r81_v",
            ObjectType::View,
            "\n\ncomment on view r81_cm.r81_v is 'It''s the view';\n\
             comment on column r81_cm.r81_v.n is 'The n';",
        ),
        (
            "r81_s",
            ObjectType::Sequence,
            "\n\ncomment on sequence r81_cm.r81_s is 'A sequence';",
        ),
        (
            "r81_f",
            ObjectType::Function,
            "\n\ncomment on function r81_cm.r81_f(x integer) is 'For ints';",
        ),
        (
            "r81_p",
            ObjectType::Procedure,
            "\n\ncomment on procedure r81_cm.r81_p() is 'A procedure';",
        ),
        ("r81_tf", ObjectType::Function, ""),
        (
            "r81_tr",
            ObjectType::Trigger,
            "\n\ncomment on trigger r81_tr on r81_cm.r81_t is 'The trigger';",
        ),
    ] {
        let ddl = ddl_of(&mut h, "r81_cm", name, object_type);
        assert!(ddl.contains(comment), "{ddl}");
        ddls.push(ddl);
    }
    assert_eq!(ddls[3].matches("comment on").count(), 1, "{}", ddls[3]);
    for ddl in &ddls {
        run_ddl(&mut h, ddl, "r81_cm.", "r81_cm_copy.");
    }
    // The comments on what is in the schema.
    let comments = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "select string_agg(o.name || ': ' || d.description, '; ' order by o.name) \
               from pg_description d \
              cross join lateral (select replace(pg_describe_object(d.classoid, d.objoid, \
                                                                    d.objsubid), \
                                                 '{schema}.', '') as name) o \
              where position('{schema}.' in pg_describe_object(d.classoid, d.objoid, \
                                                                d.objsubid)) > 0"
        ))
    };
    let was = comments(&mut h, "r81_cm");
    assert_eq!(
        was.as_deref(),
        Some(
            "column n of view r81_v: The n; function r81_f(integer): For ints; \
             function r81_p(): A procedure; sequence r81_s: A sequence; \
             sequence r81_t_id_seq: The ids; trigger r81_tr on table r81_t: The trigger; \
             view r81_v: It's the view"
        )
    );
    assert_eq!(comments(&mut h, "r81_cm_copy"), was);
    h.ok("drop schema r81_cm, r81_cm_copy cascade");
}

/// A table's DDL enables and forces its row level security where it does,
/// and creates its policies, with their command, roles, USING and WITH
/// CHECK, restrictive or permissive, and every name with its schema. Run
/// in another schema, the copy has the same (#373).
#[test]
fn a_tables_ddl_keeps_its_row_level_security() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let schema = h.schema.clone();
    h.ok("drop schema if exists r81_rls, r81_rls_copy cascade");
    h.ok("drop function if exists r81_rls_ok(int)");
    h.ok("create schema r81_rls");
    h.ok("create schema r81_rls_copy");
    h.ok(&format!(
        "create function {schema}.r81_rls_ok(x int) returns bool language sql \
         as 'select x > 0'"
    ));
    for sql in [
        "create table r81_rls.r81_log (id int)",
        "create table r81_rls.r81_t (id int primary key, who text, n int)",
        "alter table r81_rls.r81_t enable row level security",
        "alter table r81_rls.r81_t force row level security",
        "create policy r81_own on r81_rls.r81_t using (who = current_user)",
        "create policy \"R81 Insert\" on r81_rls.r81_t as restrictive for insert \
         to pg_monitor, pg_read_all_stats with check (r81_rls_ok(n))",
        "create policy r81_upd on r81_rls.r81_t for update \
         using (n in (select id from r81_rls.r81_log)) with check (n < 100)",
        "create table r81_rls.r81_on (n int)",
        "alter table r81_rls.r81_on enable row level security",
    ] {
        h.ok(sql);
    }
    let t = ddl_of(&mut h, "r81_rls", "r81_t", ObjectType::Table);
    for part in [
        "\n\nalter table r81_rls.r81_t enable row level security;\n\n\
         alter table r81_rls.r81_t force row level security;\n\n"
            .to_owned(),
        "\n\ncreate policy r81_own on r81_rls.r81_t\n  using (who = CURRENT_USER);".to_owned(),
        format!(
            "\n\ncreate policy \"R81 Insert\" on r81_rls.r81_t as restrictive for insert\n  \
             to pg_monitor, pg_read_all_stats\n  with check ({schema}.r81_rls_ok(n));"
        ),
        "\n\ncreate policy r81_upd on r81_rls.r81_t for update\n  using (".to_owned(),
        "FROM r81_rls.r81_log)))\n  with check (n < 100);".to_owned(),
    ] {
        assert!(t.contains(&part), "{t}");
    }
    let on = ddl_of(&mut h, "r81_rls", "r81_on", ObjectType::Table);
    assert!(
        on.ends_with("\n\nalter table r81_rls.r81_on enable row level security;"),
        "{on}"
    );
    let log = ddl_of(&mut h, "r81_rls", "r81_log", ObjectType::Table);
    assert!(!log.contains("row level security"), "{log}");
    for ddl in [&log, &t, &on] {
        run_ddl(&mut h, ddl, "r81_rls.", "r81_rls_copy.");
    }
    // Each table's row level security and policies.
    let security = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', c.relname, c.relrowsecurity, c.relforcerowsecurity, \
                      (select string_agg(concat_ws(' ', p.polname, p.polcmd, p.polpermissive, \
                                                   p.polroles::regrole[]::text, \
                                                   pg_get_expr(p.polqual, p.polrelid), \
                                                   pg_get_expr(p.polwithcheck, p.polrelid)), \
                                         '; ' order by p.polname) \
                         from pg_policy p where p.polrelid = c.oid)), \
                               ' | ' order by c.relname) \
               from pg_class c \
              where c.relnamespace = '{schema}'::regnamespace and c.relkind = 'r'"
        ))
        .map(|s| s.replace(&format!("{schema}."), ""))
    };
    let was = security(&mut h, "r81_rls");
    assert!(
        was.as_deref().is_some_and(|w| w.contains(
            "R81 Insert a f {pg_monitor,pg_read_all_stats} r81_rls_ok(n); r81_own * t {-}"
        )),
        "{was:?}"
    );
    assert_eq!(security(&mut h, "r81_rls_copy"), was, "{t}");
    h.ok("drop schema r81_rls, r81_rls_copy cascade");
    h.ok(&format!("drop function {schema}.r81_rls_ok(int)"));
}

/// A table's DDL gives each column its collation where it isn't its
/// type's, a domain's too, with its schema but `pg_catalog`'s: run in
/// another schema, the copy's columns compare and sort as the original's
/// (#375).
#[test]
fn a_tables_ddl_keeps_its_columns_collations() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r81_co, r81_co_copy cascade");
    h.ok("create schema r81_co");
    h.ok("create schema r81_co_copy");
    for schema in ["r81_co", "r81_co_copy"] {
        h.ok(&format!(
            "create collation {schema}.\"R81 Coll\" from \"C\""
        ));
        h.ok(&format!(
            "create domain {schema}.r81_d as text collate \"POSIX\""
        ));
    }
    h.ok(
        "create table r81_co.r81_t (a text collate \"C\", b varchar(10) collate \"POSIX\" not null, \
         c text, d r81_co.r81_d, e r81_co.r81_d collate \"C\", f text collate r81_co.\"R81 Coll\", \
         g text[] collate \"C\", n int)",
    );
    let t = ddl_of(&mut h, "r81_co", "r81_t", ObjectType::Table);
    for line in [
        "\n    a text collate \"C\",\n",
        "\n    b character varying(10) collate \"POSIX\" not null,\n",
        "\n    c text,\n",
        "\n    d r81_co.r81_d,\n",
        "\n    e r81_co.r81_d collate \"C\",\n",
        "\n    f text collate r81_co.\"R81 Coll\",\n",
        "\n    g text[] collate \"C\",\n",
        "\n    n integer\n",
    ] {
        assert!(t.contains(line), "{t}");
    }
    run_ddl(&mut h, &t, "r81_co.", "r81_co_copy.");
    let collations = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "select string_agg(a.attname || ' ' || coalesce(o.collname, '-'), ', ' order by a.attnum) \
               from pg_attribute a left join pg_collation o on o.oid = a.attcollation \
              where a.attrelid = '{schema}.r81_t'::regclass and a.attnum > 0"
        ))
    };
    let was = collations(&mut h, "r81_co");
    assert_eq!(
        was.as_deref(),
        Some("a C, b POSIX, c default, d POSIX, e C, f R81 Coll, g C, n -")
    );
    assert_eq!(collations(&mut h, "r81_co_copy"), was, "{t}");
    // What the collation means: "B" before "a" in C, after it in the
    // database's.
    for schema in ["r81_co", "r81_co_copy"] {
        h.ok(&format!(
            "insert into {schema}.r81_t (a, b) values ('a', 'a'), ('B', 'B')"
        ));
    }
    assert_eq!(
        h.scalar("select string_agg(a, '' order by a) from r81_co_copy.r81_t")
            .as_deref(),
        Some("Ba")
    );
    h.ok("drop schema r81_co, r81_co_copy cascade");
}

/// A materialized view's DDL makes it as it is: owned by its owner where
/// that isn't the role reading it, without data where it has none, with
/// its storage parameters, tablespace and access method. Run in another
/// schema by another role, the copy is the same (#330).
#[test]
fn a_materialized_view_keeps_its_owner_data_and_storage() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    let superuser = h.scalar("select rolsuper::text from pg_roles where rolname = current_user");
    if superuser.as_deref() != Some("true") {
        eprintln!("not a superuser, skipping");
        return;
    }
    h.ok("drop schema if exists r73_mv, r73_mv_copy cascade");
    h.ok("drop tablespace if exists r73_ts");
    h.ok("drop access method if exists r73_heap");
    h.ok("drop role if exists r73_owner");
    h.ok("create role r73_owner");
    h.ok("create schema r73_mv");
    h.ok("create schema r73_mv_copy");
    // A tablespace in the data directory, as tests make them.
    h.ok("set allow_in_place_tablespaces = on");
    h.ok("create tablespace r73_ts location ''");
    h.ok("reset allow_in_place_tablespaces");
    h.ok("create access method r73_heap type table handler heap_tableam_handler");
    for schema in ["r73_mv", "r73_mv_copy"] {
        h.ok(&format!("create table {schema}.r73_t (n int, t text)"));
        h.ok(&format!("insert into {schema}.r73_t values (1, 'a')"));
    }
    h.ok("create materialized view r73_mv.r73_full as select n from r73_mv.r73_t");
    h.ok("create materialized view r73_mv.r73_empty using r73_heap \
         with (fillfactor = 70, autovacuum_enabled = off, toast.autovacuum_enabled = false) \
         tablespace r73_ts as select n, t from r73_mv.r73_t with no data");
    h.ok("alter materialized view r73_mv.r73_empty owner to r73_owner");
    h.ok("create index r73_empty_n on r73_mv.r73_empty (n)");
    let full = ddl_of(&mut h, "r73_mv", "r73_full", ObjectType::MaterializedView);
    assert_eq!(
        full,
        "create materialized view r73_mv.r73_full as\n SELECT n\n   FROM r73_mv.r73_t;"
    );
    let empty = ddl_of(&mut h, "r73_mv", "r73_empty", ObjectType::MaterializedView);
    assert!(
        empty.starts_with(
            "create materialized view r73_mv.r73_empty using r73_heap \
             with (fillfactor='70', autovacuum_enabled=off, toast.autovacuum_enabled='false') \
             tablespace r73_ts as\n SELECT n,\n    t\n   FROM r73_mv.r73_t\nwith no data;\n\n\
             alter materialized view r73_mv.r73_empty owner to r73_owner;\n\n\
             CREATE INDEX r73_empty_n ON r73_mv.r73_empty"
        ),
        "{empty}"
    );
    for ddl in [&full, &empty] {
        run_ddl(&mut h, ddl, "r73_mv.r73_", "r73_mv_copy.r73_");
    }
    let state = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', c.relname, c.relowner::regrole, c.relispopulated, \
                                         c.reloptions, s.reloptions, coalesce(t.spcname, '-'), \
                                         a.amname, i.indexes), \
                               '; ' order by c.relname) \
               from pg_class c join pg_am a on a.oid = c.relam \
               left join pg_class s on s.oid = c.reltoastrelid \
               left join pg_tablespace t on t.oid = c.reltablespace \
              cross join lateral (select count(*) as indexes from pg_index \
                                   where indrelid = c.oid) i \
              where c.relnamespace = '{schema}'::regnamespace and c.relkind = 'm'"
        ))
    };
    let me = h
        .scalar("select current_user::regrole::text")
        .unwrap_or_default();
    let was = state(&mut h, "r73_mv");
    assert_eq!(
        was,
        Some(format!(
            "r73_empty r73_owner f {{fillfactor=70,autovacuum_enabled=off}} \
             {{autovacuum_enabled=false}} r73_ts r73_heap 1; r73_full {me} t - heap 0"
        ))
    );
    assert_eq!(state(&mut h, "r73_mv_copy"), was, "{empty}");
    h.ok("drop schema r73_mv, r73_mv_copy cascade");
    h.ok("drop tablespace r73_ts");
    h.ok("drop access method r73_heap");
    h.ok("drop role r73_owner");
}

/// The DDL of a table, a view, a sequence and a routine has their grants,
/// as a materialized view's has (#301): run in another schema, the copies
/// have the same privileges, the table's serial sequence too, and a
/// routine's EXECUTE that PUBLIC no longer has stays revoked (#331).
#[test]
fn the_ddl_has_the_grants() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r73_g, r73_g_copy cascade");
    h.ok("drop role if exists \"R73 Reader\"");
    h.ok("create role \"R73 Reader\"");
    h.ok("create schema r73_g");
    h.ok("create schema r73_g_copy");
    for sql in [
        "create table r73_g.r73_t (id serial, n int, m int)",
        "grant select, insert on r73_g.r73_t to \"R73 Reader\"",
        "grant update (n), select (m) on r73_g.r73_t to pg_monitor with grant option",
        "grant select on r73_g.r73_t to public",
        "revoke truncate on r73_g.r73_t from current_user",
        "grant usage on sequence r73_g.r73_t_id_seq to \"R73 Reader\"",
        "create view r73_g.r73_v as select n from r73_g.r73_t",
        "grant select on r73_g.r73_v to \"R73 Reader\"",
        "grant select (n) on r73_g.r73_v to pg_monitor",
        "create sequence r73_g.r73_s",
        "grant usage, select on sequence r73_g.r73_s to \"R73 Reader\" with grant option",
        "grant update on sequence r73_g.r73_s to public",
        "create function r73_g.r73_f(x int) returns int language sql as 'select x'",
        "revoke execute on function r73_g.r73_f(int) from public",
        "grant execute on function r73_g.r73_f(int) to \"R73 Reader\"",
        "create function r73_g.r73_f(x text) returns text language sql as 'select x'",
        "create procedure r73_g.r73_p() language sql as 'select 1'",
        "grant execute on procedure r73_g.r73_p() to pg_monitor with grant option",
    ] {
        h.ok(sql);
    }
    let table = ddl_of(&mut h, "r73_g", "r73_t", ObjectType::Table);
    for grant in [
        "grant insert, select on r73_g.r73_t to \"R73 Reader\";",
        "grant update (n), select (m) on r73_g.r73_t to pg_monitor with grant option;",
        "grant select on r73_g.r73_t to public;",
        "revoke truncate on r73_g.r73_t from ",
        "grant usage on sequence r73_g.r73_t_id_seq to \"R73 Reader\";",
    ] {
        assert!(table.contains(grant), "{table}");
    }
    let function = ddl_of(&mut h, "r73_g", "r73_f", ObjectType::Function);
    for grant in [
        "grant execute on function r73_g.r73_f(x integer) to \"R73 Reader\";",
        "revoke execute on function r73_g.r73_f(x integer) from public;",
    ] {
        assert!(function.contains(grant), "{function}");
    }
    assert!(
        !function.contains("on function r73_g.r73_f(x text)"),
        "{function}"
    );
    for (name, object_type) in [
        ("r73_t", ObjectType::Table),
        ("r73_v", ObjectType::View),
        ("r73_s", ObjectType::Sequence),
        ("r73_f", ObjectType::Function),
        ("r73_p", ObjectType::Procedure),
    ] {
        let ddl = ddl_of(&mut h, "r73_g", name, object_type);
        run_ddl(&mut h, &ddl, "r73_g.", "r73_g_copy.");
    }
    // Each object's and column's privileges, and who has them.
    let privileges = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "with o (name, acl) as ( \
               select relname, relacl from pg_class \
                where relnamespace = '{schema}'::regnamespace \
               union all \
               select c.relname || '.' || a.attname, a.attacl \
                 from pg_class c join pg_attribute a on a.attrelid = c.oid \
                where c.relnamespace = '{schema}'::regnamespace and a.attnum > 0 \
               union all \
               select proname || '(' || pg_get_function_identity_arguments(oid) || ')', proacl \
                 from pg_proc where pronamespace = '{schema}'::regnamespace) \
             select string_agg(name || ' ' || coalesce(( \
                      select string_agg(x.grantee::regrole || ' ' || x.privilege_type \
                                        || case when x.is_grantable then '*' else '' end, \
                                        ', ' order by x.grantee::regrole::text, \
                                                      x.privilege_type) \
                        from aclexplode(acl) x), 'default'), '; ' order by name) \
               from o"
        ))
    };
    let was = privileges(&mut h, "r73_g");
    assert!(
        was.as_deref()
            .is_some_and(|p| p.contains("\"R73 Reader\" USAGE; r73_t_id_seq.is_called")),
        "{was:?}"
    );
    assert_eq!(privileges(&mut h, "r73_g_copy"), was, "{table}");
    h.ok("drop schema r73_g, r73_g_copy cascade");
    h.ok("drop role \"R73 Reader\"");
}

/// An identity column's sequence is made by its table's DDL: the
/// sequence's own DDL says so and sets the identity's options, so that run
/// next to a copy of the table it gives the copy's identity the same ones
/// rather than making a sequence of its own; next to a table without the
/// identity it fails (#332).
#[test]
fn an_identitys_sequence_is_made_by_its_table() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop schema if exists r73_is, r73_is_copy cascade");
    h.ok("create schema r73_is");
    h.ok("create schema r73_is_copy");
    h.ok(
        "create table r73_is.r73_t (id int generated always as identity \
         (start with 5 increment by 3 maxvalue 1000 cache 2 cycle), \
         b bigint generated by default as identity (sequence name r73_is.\"R73 B\"), n int)",
    );
    h.ok("grant select on sequence r73_is.r73_t_id_seq to pg_monitor");
    let id = ddl_of(&mut h, "r73_is", "r73_t_id_seq", ObjectType::Sequence);
    assert_eq!(
        id,
        "-- The sequence of the identity column r73_is.r73_t.id, made by the table's DDL.\n\
         alter table r73_is.r73_t alter column id\n  \
         set increment by 3 set minvalue 1 set maxvalue 1000\n  \
         set start with 5 set cache 2 set cycle;\n\n\
         grant select on sequence r73_is.r73_t_id_seq to pg_monitor;"
    );
    let b = ddl_of(&mut h, "r73_is", "R73 B", ObjectType::Sequence);
    assert!(
        b.contains(
            "alter table r73_is.r73_t alter column b\n  \
             set increment by 1 set minvalue 1 set maxvalue 9223372036854775807\n  \
             set start with 1 set cache 1 set no cycle;"
        ),
        "{b}"
    );
    h.ok(
        "create table r73_is_copy.r73_t (id int generated always as identity, \
         b bigint generated by default as identity (sequence name r73_is_copy.\"R73 B\"), n int)",
    );
    for ddl in [&id, &b] {
        run_ddl(&mut h, ddl, "r73_is.", "r73_is_copy.");
    }
    let sequences = |h: &mut Harness, schema: &str| {
        h.scalar(&format!(
            "select string_agg(concat_ws(' ', c.relname, s.seqtypid::regtype, s.seqstart, \
                                         s.seqincrement, s.seqmin, s.seqmax, s.seqcache, \
                                         s.seqcycle, c.relacl), \
                               '; ' order by c.relname) \
               from pg_sequence s join pg_class c on c.oid = s.seqrelid \
              where c.relnamespace = '{schema}'::regnamespace"
        ))
    };
    let was = sequences(&mut h, "r73_is");
    assert_eq!(sequences(&mut h, "r73_is_copy"), was, "{id}");
    // Without the identity, there is nothing to set.
    h.ok("create table r73_is_copy.r73_plain (id int)");
    h.fails(
        id.replace("r73_is.r73_t ", "r73_is_copy.r73_plain ")
            .split(";\n")
            .next()
            .unwrap_or_default(),
    );
    h.ok("drop schema r73_is, r73_is_copy cascade");
}

/// Object filters find names without case whatever their letters: names
/// collate as C, where upper() changes only ASCII letters, in the test
/// database and in one whose lc_ctype is C.
#[test]
fn filters_match_any_letters_without_case() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::SetAutocommit(true));
    h.ok("drop database if exists r53_c");
    h.ok(
        "create database r53_c with encoding 'UTF8' lc_collate 'C' lc_ctype 'C' \
         template template0",
    );
    let mut params = params().expect("the test database");
    let (server, _) = params.connect_string.rsplit_once('/').expect("a database");
    params.connect_string = format!("{server}/r53_c");
    let mut c = Harness::with(params);
    c.session.send(DbRequest::SetAutocommit(true));
    for h in [&mut h, &mut c] {
        let schema = h.schema.clone();
        let tables = "\"r53_čaj\", \"r53_bière\", \"r53_ÖL\", r53_tea";
        h.ok(&format!("drop table if exists {tables}"));
        h.ok("create table \"r53_čaj\" (x int)");
        h.ok("create table \"r53_bière\" (x int)");
        h.ok("create table \"r53_ÖL\" (x int)");
        h.ok("create table r53_tea (x int)");
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
        assert_eq!(filtered("čaj"), ["r53_čaj"]);
        assert_eq!(filtered("ČAJ"), ["r53_čaj"]);
        assert_eq!(filtered("r53_Č"), ["r53_čaj"]);
        assert_eq!(filtered("BIÈRE"), ["r53_bière"]);
        assert_eq!(filtered("öl"), ["r53_ÖL"]);
        assert_eq!(filtered("R53_?A"), ["r53_čaj"]);
        assert_eq!(filtered("r53_?EA"), ["r53_tea"]);
        assert_eq!(filtered("r53*è"), ["r53_bière"]);
        assert_eq!(filtered("r53%"), Vec::<String>::new());
        assert_eq!(filtered("r53.t"), Vec::<String>::new());
        let MetaResult::Objects(found) = h.meta(MetaQuery::Find {
            pattern: "R53_ČA".into(),
            limit: 10,
        }) else {
            panic!("expected objects")
        };
        let found: Vec<&str> = found.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(found, ["r53_čaj"]);
        h.ok(&format!("drop table {tables}"));
    }
    drop(c);
    h.ok("drop database r53_c with (force)");
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

/// Roles that can log in are users, the others roles.
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
    h.ok("drop role if exists meta_role");
    h.ok("drop role if exists \"Meta_User\"");
    h.ok("create role meta_role");
    h.ok("create role \"Meta_User\" login");

    let users = accounts(&mut h, ObjectType::User);
    assert!(names(&users).contains(&"Meta_User".to_owned()), "{users:?}");
    assert!(
        !names(&users).contains(&"meta_role".to_owned()),
        "{users:?}"
    );
    let me = h.scalar("select current_user").expect("the current user");
    let super_user = h.scalar("select rolsuper::text from pg_roles where rolname = current_user");
    if super_user.as_deref() == Some("true") {
        assert!(users.contains(&(me, "superuser".into())), "{users:?}");
    }
    let roles = names(&accounts(&mut h, ObjectType::Role));
    assert!(roles.contains(&"meta_role".to_owned()), "{roles:?}");
    assert!(!roles.contains(&"Meta_User".to_owned()), "{roles:?}");

    h.ok("drop role meta_role");
    h.ok("drop role \"Meta_User\"");
}

/// Through the SSH server (#97): trusting its key, sessions sharing the
/// tunnel, Break (its cancel request goes through the tunnel too), and an
/// address the SSH server can't reach.
#[test]
fn through_an_ssh_tunnel() {
    let dir = std::env::temp_dir().join(format!("pythia-postgres-ssh-{}", std::process::id()));
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
    // A second session rides the same SSH connection.
    let mut other = Harness::with(params.clone());
    assert_eq!(
        other.scalar("select current_database()").as_deref(),
        Some("pythia")
    );

    h.execute(1, "select pg_sleep(30)", 10);
    std::thread::sleep(Duration::from_millis(500));
    h.session.cancel();
    let error = loop {
        match h.next_result() {
            DbEvent::QueryStarted { .. } => {}
            DbEvent::Failed { error, .. } => break error,
            other => panic!("expected Failed, got {other:?}"),
        }
    };
    assert!(error.message.contains("57014"), "{error}");
    h.settle();
    assert_eq!(h.scalar("select 1").as_deref(), Some("1"));

    // An address the SSH server can't reach: the tunnel says why.
    let unreachable = ConnectParams {
        connect_string: "nosuchhost.invalid:5432/pythia".into(),
        ..params
    };
    let error = connect_error(unreachable);
    assert!(
        error
            .message
            .contains("the server couldn't connect to nosuchhost.invalid:5432"),
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

/// Bytes bind as bytea and text as text, by the extended protocol, whole:
/// megabytes into a bytea and a text, inserted and updated (#293).
#[test]
fn values_bound_whole() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists lob_bound");
    h.ok("create table lob_bound (id int primary key, b bytea, t text)");
    let (first, second) = (whole_values(0), whole_values(1));
    let insert = "insert into lob_bound (id, b, t) values (1, :b, :t)";
    assert_eq!(h.bound(insert, binds(&first, &["b", "t"])), Ok(Some(1)));
    let update = "update lob_bound set b = :b, t = :t where id = 1";
    assert_eq!(h.bound(update, binds(&second, &["b", "t"])), Ok(Some(1)));
    let row = h.whole_row("select b, t from lob_bound");
    assert!(unhex(row[0].as_deref().unwrap()) == second.0, "b");
    assert!(row[1].as_deref() == Some(second.1.as_str()), "t");
    // The changes' transaction would keep the table.
    h.ok("rollback");
    h.ok("drop table if exists lob_bound");
}
