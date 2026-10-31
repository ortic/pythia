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
    ConnectParams, DbEvent, DbKind, DbRequest, Detail, Dialect, MetaQuery, MetaResult, ObjectType,
    Session,
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
        binds: vec![("A".into(), Some("41".into())), ("b".into(), None)],
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
    assert_eq!(h.scalar("select 'next'").as_deref(), Some("next"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    // Fetching more of the closed result reports that there is no more.
    h.session.send(DbRequest::FetchMore {
        id: 1,
        max_rows: 10,
    });
    let DbEvent::Rows { id, rows, more, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!((id, rows.len(), more), (1, 0, false));

    // A small result that is closed early needs no cancel.
    h.execute(2, "select n from generate_series(1, 30) n", 10);
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));

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
        ddl.contains("constraint meta_emp_fk FOREIGN KEY (deptno) REFERENCES meta_dept(deptno)"),
        "{ddl}"
    );
    assert!(ddl.contains("CREATE INDEX meta_emp_dept ON"), "{ddl}");
    // The DDL runs: it rebuilds the table elsewhere.
    h.ok("create schema if not exists meta_copy");
    for statement in pythia_plsql::split_statements_in(&ddl, pythia_plsql::Syntax::Postgres) {
        let sql = statement
            .text(&ddl)
            .replace(&format!("{schema}."), "meta_copy.");
        h.ok(&sql);
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

    h.ok("drop view meta_v");
    h.ok("drop table meta_emp");
    h.ok("drop table meta_dept");
    h.ok("drop function meta_f(int)");
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
