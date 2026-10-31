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
    ConnectParams, DbEvent, DbKind, DbRequest, Detail, Dialect, MetaQuery, MetaResult, ObjectType,
    Session,
};

fn params() -> Option<ConnectParams> {
    from_spec(&std::env::var("PYTHIA_TEST_MSSQL").ok()?)
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
        binds: vec![("A".into(), Some("41".into())), ("b".into(), None)],
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

    h.execute(2, &numbers(30), 10);
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));

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

#[test]
fn several_result_sets() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.execute(1, "select 1 as a; select 2 as b; select 3 as c", 10);
    let mut notices = Vec::new();
    let mut rows = None;
    loop {
        match h.next() {
            DbEvent::Rows { rows: r, .. } => rows = Some(r),
            DbEvent::Notice(n) => notices.push(n),
            DbEvent::Transaction { .. } => break,
            DbEvent::Failed { error, .. } => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(rows, Some(vec![vec![Some("1".to_owned())]]));
    assert_eq!(
        notices,
        ["The batch returned 2 more result set(s); only the first is shown"]
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
    h.ok("drop view if exists meta_v");
    h.ok("drop procedure if exists meta_p");
    h.ok("drop table if exists meta_emp");
    h.ok("drop table if exists meta_dept");
    h.ok("create table meta_dept (deptno int primary key, name varchar(30) not null)");
    h.ok(
        "exec sp_addextendedproperty 'MS_Description', 'Department', 'schema', 'dbo', \
         'table', 'meta_dept', 'column', 'name'",
    );
    h.ok(
        "create table meta_emp (empno int identity(1, 1) primary key, \
         deptno int constraint meta_emp_fk references meta_dept (deptno), \
         [Salary] decimal(8,2) not null constraint meta_emp_sal default 0, \
         note nvarchar(max))",
    );
    h.ok("create index meta_emp_dept on meta_emp (deptno)");
    h.ok("create view meta_v as select e.empno, d.name from dbo.meta_emp e join dbo.meta_dept d on d.deptno = e.deptno");
    h.ok("create procedure meta_p @p int as begin select @p; end");

    let MetaResult::Schemas(schemas) = h.meta(MetaQuery::Schemas) else {
        panic!("expected schemas")
    };
    assert!(schemas.contains(&schema), "{schemas:?}");
    assert!(!schemas.contains(&"db_owner".to_owned()), "{schemas:?}");

    let MetaResult::Objects(tables) = h.meta(MetaQuery::Objects {
        owner: schema.clone(),
        object_type: ObjectType::Table,
        filter: Some("META_".into()),
    }) else {
        panic!("expected objects")
    };
    let names: Vec<&str> = tables.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["meta_dept", "meta_emp"]);
    assert_eq!(tables[0].last_ddl.len(), 19, "{:?}", tables[0]);

    let MetaResult::Objects(procedures) = h.meta(MetaQuery::Objects {
        owner: schema.clone(),
        object_type: ObjectType::Procedure,
        filter: Some("meta".into()),
    }) else {
        panic!("expected objects")
    };
    assert_eq!(procedures.len(), 1, "{procedures:?}");

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
    assert_eq!(columns[1].data_type, "varchar(30)");
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
    assert_eq!(fk[3].as_deref(), Some("dbo.meta_dept"));

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
            "create table [{schema}].[meta_emp] (\n    [empno] int identity(1, 1) not null,"
        )),
        "{ddl}"
    );
    assert!(
        ddl.contains("[Salary] decimal(8,2) not null constraint [meta_emp_sal] default ((0))"),
        "{ddl}"
    );
    assert!(ddl.contains("[note] nvarchar(max) null"), "{ddl}");
    assert!(
        ddl.contains(
            "constraint [meta_emp_fk] foreign key ([deptno]) references [dbo].[meta_dept] ([deptno])"
        ),
        "{ddl}"
    );
    assert!(
        ddl.contains("create nonclustered index [meta_emp_dept] on"),
        "{ddl}"
    );
    assert!(ddl.ends_with("\nGO\n"), "{ddl}");
    // The DDL runs: it rebuilds the table elsewhere.
    h.ok("if schema_id('meta_copy') is null exec('create schema meta_copy')");
    for statement in pythia_plsql::split_statements_in(&ddl, pythia_plsql::Syntax::TSql) {
        let sql = statement
            .text(&ddl)
            .replace(&format!("[{schema}].[meta_emp]"), "[meta_copy].[meta_emp]")
            .replace("[meta_emp_sal]", "[meta_copy_sal]")
            .replace("[meta_emp_fk]", "[meta_copy_fk]")
            .replace("[PK__", "[PK_copy__");
        h.ok(&sql);
    }
    h.ok("drop table meta_copy.meta_emp");
    h.ok("drop schema meta_copy");

    let MetaResult::Text(source) = h.meta(MetaQuery::Source {
        owner: schema.clone(),
        name: "meta_p".into(),
        object_type: ObjectType::Procedure,
    }) else {
        panic!("expected source")
    };
    assert_eq!(
        source,
        "create procedure meta_p @p int as begin select @p; end"
    );

    let MetaResult::Objects(found) = h.meta(MetaQuery::Find {
        pattern: "meta_*".into(),
        limit: 10,
    }) else {
        panic!("expected objects")
    };
    assert!(
        found
            .iter()
            .any(|o| o.name == "meta_p" && o.object_type == ObjectType::Procedure)
    );
    assert_eq!(found[0].owner, schema, "the default schema comes first");

    let MetaResult::Table { columns, rows } = h.meta(MetaQuery::ExplainPlan {
        sql: "select * from meta_emp e join meta_dept d on d.deptno = e.deptno \
              where e.empno = :id"
            .into(),
    }) else {
        panic!("expected a plan")
    };
    assert_eq!(columns[0].name, "ID");
    assert_eq!(rows[0][3].as_deref(), Some("SELECT"), "{rows:?}");
    assert!(
        rows.iter()
            .any(|r| r[4].as_deref().is_some_and(|o| o.starts_with("dbo.meta_"))),
        "{rows:?}"
    );
    // SHOWPLAN is off again: statements run.
    assert_eq!(h.scalar("select 'runs'").as_deref(), Some("runs"));

    h.ok("drop view meta_v");
    h.ok("drop procedure meta_p");
    h.ok("drop table meta_emp");
    h.ok("drop table meta_dept");
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
