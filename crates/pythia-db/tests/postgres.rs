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
