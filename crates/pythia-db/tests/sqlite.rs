//! SQLite sessions against database files of their own in the temp
//! directory: these run everywhere, no server needed.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use pythia_db::{
    ConnectParams, DbEvent, DbKind, DbRequest, Detail, Dialect, MetaQuery, MetaResult, ObjectType,
    Session,
};

/// A new database file for one test, removed when it ends.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("pythia-sqlite-{}-{name}.db", std::process::id()));
        let db = Self(path);
        db.remove();
        db
    }

    fn remove(&self) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }

    /// The connect string that makes the file on first use.
    fn create(&self) -> String {
        format!("{}?create", self.0.display())
    }

    fn path(&self) -> String {
        self.0.display().to_string()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

fn params(connect_string: &str) -> ConnectParams {
    ConnectParams {
        kind: DbKind::Sqlite,
        username: String::new(),
        password: String::new(),
        connect_string: connect_string.into(),
        privilege: Default::default(),
        init_sql: Vec::new(),
        ssh: None,
    }
}

struct Harness {
    session: Session,
    wake: mpsc::Receiver<()>,
}

impl Harness {
    fn connect(connect_string: &str) -> Self {
        match Self::try_connect(params(connect_string)) {
            Ok(h) => h,
            Err(error) => panic!("connect failed: {error}"),
        }
    }

    fn try_connect(params: ConnectParams) -> Result<Self, String> {
        let (tx, wake) = mpsc::channel();
        let session = Session::connect(params, move || {
            let _ = tx.send(());
        });
        let mut h = Self { session, wake };
        match h.next() {
            DbEvent::Connected(server) => {
                assert_eq!(server.dialect, Dialect::SQLITE);
                assert!(server.release >= (3, 38), "{server:?}");
                assert!(server.banner.starts_with("SQLite 3."), "{server:?}");
                assert_eq!(server.current_schema, "main");
                Ok(h)
            }
            DbEvent::ConnectFailed(error) => Err(error.text()),
            other => panic!("unexpected {other:?}"),
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

    /// Runs a statement that must fail; returns its message.
    fn fails(&mut self, sql: &str) -> String {
        self.failing(sql).0
    }

    /// Runs a statement that must fail; returns its message and whether a
    /// transaction is pending after it.
    fn failing(&mut self, sql: &str) -> (String, bool) {
        self.execute(0, sql, 100);
        let DbEvent::Failed { error, .. } = self.next_result() else {
            panic!("{sql} should fail")
        };
        (error.text(), self.settle())
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

    /// A generic table's rows of a meta query.
    fn table(&mut self, query: MetaQuery) -> Vec<Vec<Option<String>>> {
        match self.meta(query) {
            MetaResult::Table { rows, .. } => rows,
            other => panic!("expected a table, got {other:?}"),
        }
    }
}

#[test]
fn a_missing_file_is_only_created_when_asked() {
    let db = TempDb::new("missing");
    let error = Harness::try_connect(params(&db.path()))
        .err()
        .expect("no file, no connection");
    assert!(error.contains("There is no database file"), "{error}");
    assert!(error.contains("?create"), "{error}");
    assert!(!db.0.exists(), "nothing was made");

    let mut h = Harness::connect(&db.create());
    h.ok("create table t (a)");
    assert!(db.0.exists());

    // Not a database: said on connect, not on the first query.
    let text = TempDb::new("text");
    std::fs::write(&text.0, "just some text, long enough to have a header ...").unwrap();
    let error = Harness::try_connect(params(&text.path()))
        .err()
        .expect("not a database");
    assert!(error.contains("not a database"), "{error}");
}

#[test]
fn query_and_fetch_more() {
    let db = TempDb::new("fetch");
    let mut h = Harness::connect(&db.create());
    h.execute(
        1,
        "with recursive n(n) as (select 1 union all select n + 1 from n where n < 25) \
         select n, 'row ' || n as label from n",
        10,
    );
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
    h.settle();

    // Another request closes an open result.
    h.execute(2, "select value from json_each('[1,2,3]')", 1);
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));
    assert_eq!(h.scalar("select 'next'").as_deref(), Some("next"));
}

#[test]
fn values_types_and_binds() {
    let db = TempDb::new("values");
    let mut h = Harness::connect(&db.create());
    h.ok(
        "create table v (i integer, r real, d decimal(10,2), t varchar(20), b blob, \
                         dt date, n)",
    );
    h.ok(
        "insert into v values (7, 1.0, 12.5, 'grüße', x'00ff', '2026-10-31', null), \
                              (-1, 0.1, 3, 'x', null, '2026-11-01', 'mixed')",
    );
    let DbEvent::QueryStarted { columns, .. } = h.run(1, "select v.*, i * 2 as twice from v")
    else {
        panic!("expected QueryStarted")
    };
    let types: Vec<(&str, bool)> = columns
        .iter()
        .map(|c| (c.type_name.as_str(), c.numeric))
        .collect();
    assert_eq!(
        types,
        [
            ("INTEGER", true),
            ("REAL", true),
            ("DECIMAL(10,2)", true),
            ("VARCHAR(20)", false),
            ("BLOB", false),
            // Text in a column of NUMERIC affinity: left aligned.
            ("DATE", false),
            // No declared type: its values'.
            ("TEXT", false),
            ("INTEGER", true),
        ]
    );
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(
        rows[0],
        vec![
            Some("7".into()),
            Some("1.0".into()),
            Some("12.5".into()),
            Some("grüße".into()),
            Some("00FF".into()),
            Some("2026-10-31".into()),
            None,
            Some("14".into()),
        ]
    );
    h.settle();

    h.session.send(DbRequest::Execute {
        id: 2,
        sql: "select count(*), :B is null, :a, 'it''s :a' from v where i = :A".into(),
        max_rows: 10,
        binds: vec![("A".into(), Some("7".into())), ("b".into(), None)],
    });
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    // The text '7' finds the integer 7: the column's affinity converts it.
    assert_eq!(
        rows[0],
        vec![
            Some("1".into()),
            Some("1".into()),
            Some("7".into()),
            Some("it's :a".into())
        ]
    );
}

#[test]
fn errors_point_at_the_statement() {
    let db = TempDb::new("errors");
    let mut h = Harness::connect(&db.create());
    let DbEvent::Failed { error, .. } = h.run(1, "select * from no_such_table") else {
        panic!("expected Failed")
    };
    assert_eq!(error.text(), "no such table: no_such_table");
    assert_eq!(error.offset, Some(14));
    assert!(!error.is_connection_lost());
    h.settle();

    let DbEvent::Failed { error, .. } = h.run(2, "select *\n  frm t") else {
        panic!("expected Failed")
    };
    assert!(error.text().contains("syntax error"), "{}", error.text());
    assert_eq!(error.offset, Some(11));
    h.settle();

    h.ok("create table u (id integer primary key, p integer references u (id))");
    let unique = h.fails("insert into u values (1, null), (1, null)");
    assert_eq!(unique, "UNIQUE constraint failed: u.id");
    // Foreign keys are checked.
    let foreign = h.fails("insert into u values (2, 99)");
    assert_eq!(foreign, "FOREIGN KEY constraint failed");
    assert!(
        h.fails("select 1; select 2")
            .contains("More than one statement"),
        "one statement at a time"
    );
}

#[test]
fn transactions_like_oracle() {
    let db = TempDb::new("tx");
    let mut h = Harness::connect(&db.create());
    // DDL outside a transaction is committed at once.
    assert!(!h.ok("create table t (a integer primary key)"));
    assert!(h.ok("insert into t values (1)"));
    h.session.send(DbRequest::Rollback);
    assert!(!h.settle());
    assert_eq!(h.scalar("select count(*) from t").as_deref(), Some("0"));

    // A failed change of its own leaves no transaction behind.
    assert!(!h.failing("insert into t values (5), (5)").1);
    assert_eq!(h.scalar("select count(*) from t").as_deref(), Some("0"));
    // In one, it is undone alone: the transaction goes on.
    assert!(h.ok("insert into t values (5)"));
    assert!(h.failing("insert into t values (5)").1);

    // Another session sees the change once it is committed.
    let mut other = Harness::connect(&db.path());
    assert_eq!(other.scalar("select count(*) from t").as_deref(), Some("0"));
    h.session.send(DbRequest::Commit);
    assert!(!h.settle());
    assert_eq!(other.scalar("select count(*) from t").as_deref(), Some("1"));

    // DDL in an open transaction waits for Commit like the rest.
    assert!(h.ok("insert into t values (6)"));
    assert!(h.ok("create table later (x)"));
    h.session.send(DbRequest::Rollback);
    assert!(!h.settle());
    assert!(h.fails("select * from later").contains("no such table"));

    // The session's own BEGIN and COMMIT.
    assert!(h.ok("begin"));
    assert!(h.ok("update t set a = 7 where a = 5"));
    assert!(!h.ok("commit"));

    // Autocommit: every change is committed on its own.
    h.session.send(DbRequest::SetAutocommit(true));
    assert!(!h.ok("delete from t"));
    assert_eq!(other.scalar("select count(*) from t").as_deref(), Some("0"));

    // Nothing to commit: no error.
    h.session.send(DbRequest::Commit);
    assert!(matches!(h.next(), DbEvent::Notice(_)));
    assert!(!h.settle());
}

#[test]
fn break_stops_a_query() {
    let db = TempDb::new("cancel");
    let mut h = Harness::connect(&db.create());
    h.execute(
        1,
        "with recursive n(n) as (select 1 union all select n + 1 from n) \
         select count(*) from n",
        10,
    );
    std::thread::sleep(Duration::from_millis(300));
    h.session.cancel();
    match h.next_result() {
        DbEvent::Failed { error, .. } => assert_eq!(error.text(), "Cancelled"),
        DbEvent::QueryStarted { .. } => match h.next_result() {
            DbEvent::Failed { error, .. } => assert_eq!(error.text(), "Cancelled"),
            other => panic!("expected the cancel, got {other:?}"),
        },
        other => panic!("expected the cancel, got {other:?}"),
    }
    h.settle();
    // The session goes on.
    assert_eq!(h.scalar("select 42").as_deref(), Some("42"));
}

#[test]
fn a_trigger_runs_as_one_statement() {
    let db = TempDb::new("trigger");
    let mut h = Harness::connect(&db.create());
    let script = "create table t (id integer primary key, name text);\n\
                  create table audit (id, name);\n\
                  create trigger t_audit after update of name on t\n\
                  begin\n  insert into audit values (old.id, old.name);\n  \
                  insert into audit values (new.id, new.name);\nend;\n\
                  insert into t values (1, 'a');\n\
                  update t set name = 'b';";
    let statements = pythia_plsql::split_statements_in(script, Dialect::SQLITE.syntax());
    assert_eq!(statements.len(), 5);
    for statement in &statements {
        h.ok(statement.text(script));
    }
    assert_eq!(
        h.scalar("select group_concat(name, ',') from audit")
            .as_deref(),
        Some("a,b")
    );
}

#[test]
fn dictionary() {
    let db = TempDb::new("dictionary");
    let mut h = Harness::connect(&db.create());
    for sql in [
        "create table dept (id integer primary key, name text not null unique)",
        "create table emp (id integer primary key, dept_id integer not null \
            references dept (id) on delete cascade, name text default 'n/a', \
            salary real, unique (dept_id, name))",
        "create index emp_salary on emp (salary desc)",
        "create view rich as select e.name, d.name as dept from emp e join dept d \
            on d.id = e.dept_id where e.salary > 100",
        "create trigger emp_ins before insert on emp begin select 1; end",
        "create table \"order\" (\"key\" text, b, primary key (\"key\", b)) without rowid",
    ] {
        h.ok(sql);
    }
    let MetaResult::Schemas(schemas) = h.meta(MetaQuery::Schemas) else {
        panic!()
    };
    assert_eq!(schemas, ["main"]);

    let objects = |h: &mut Harness, object_type, filter: Option<&str>| {
        let MetaResult::Objects(objects) = h.meta(MetaQuery::Objects {
            owner: "main".into(),
            object_type,
            filter: filter.map(str::to_owned),
        }) else {
            panic!()
        };
        objects.into_iter().map(|o| o.name).collect::<Vec<_>>()
    };
    assert_eq!(
        objects(&mut h, ObjectType::Table, None),
        ["dept", "emp", "order"]
    );
    assert_eq!(objects(&mut h, ObjectType::Table, Some("m*p")), ["emp"]);
    assert_eq!(objects(&mut h, ObjectType::View, None), ["rich"]);
    // Not the autoindexes of the unique constraints.
    assert_eq!(objects(&mut h, ObjectType::Index, None), ["emp_salary"]);
    assert_eq!(objects(&mut h, ObjectType::Trigger, None), ["emp_ins"]);

    let MetaResult::Columns(columns) = h.meta(MetaQuery::Columns {
        owner: "main".into(),
        table: "EMP".into(),
        keys: true,
    }) else {
        panic!()
    };
    let summary: Vec<String> = columns
        .iter()
        .map(|c| {
            format!(
                "{} {} null={} default={:?} pk={:?}",
                c.name, c.data_type, c.nullable, c.default, c.pk_position
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            "id INTEGER null=false default=None pk=Some(1)",
            "dept_id INTEGER null=false default=None pk=None",
            "name TEXT null=true default=Some(\"'n/a'\") pk=None",
            "salary REAL null=true default=None pk=None",
        ]
    );
    let MetaResult::Columns(columns) = h.meta(MetaQuery::Columns {
        owner: "main".into(),
        table: "order".into(),
        keys: true,
    }) else {
        panic!()
    };
    assert_eq!(
        columns.iter().map(|c| c.pk_position).collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );

    let details = |h: &mut Harness, name: &str, detail| {
        h.table(MetaQuery::Details {
            owner: "main".into(),
            name: name.into(),
            detail,
        })
    };
    let indexes = details(&mut h, "emp", Detail::Indexes);
    let index = |name: &str| {
        indexes
            .iter()
            .find(|r| r[0].as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name} in {indexes:?}"))
            .clone()
    };
    assert_eq!(index("emp_salary")[3].as_deref(), Some("salary"));
    assert_eq!(
        index("sqlite_autoindex_emp_1")[1..4],
        [
            Some("Unique constraint".into()),
            Some("UNIQUE".into()),
            Some("dept_id, name".into())
        ]
    );
    let constraints = details(&mut h, "emp", Detail::Constraints);
    let kinds: Vec<(Option<&str>, Option<&str>, Option<&str>)> = constraints
        .iter()
        .map(|r| (r[1].as_deref(), r[2].as_deref(), r[3].as_deref()))
        .collect();
    assert_eq!(
        kinds,
        [
            (Some("Primary key"), Some("id"), None),
            (Some("Unique"), Some("dept_id, name"), None),
            (Some("Foreign key"), Some("dept_id"), Some("dept (id)")),
        ]
    );
    assert_eq!(constraints[2][4].as_deref(), Some("CASCADE"));
    let triggers = details(&mut h, "emp", Detail::Triggers);
    assert_eq!(
        triggers,
        [vec![Some("emp_ins".into()), Some("BEFORE INSERT".into())]]
    );
    let used_by = details(&mut h, "dept", Detail::UsedBy);
    assert_eq!(
        used_by
            .iter()
            .map(|r| (r[1].as_deref(), r[2].as_deref()))
            .collect::<Vec<_>>(),
        [(Some("rich"), Some("VIEW")), (Some("emp"), Some("TABLE"))]
    );
    let uses = details(&mut h, "rich", Detail::Uses);
    assert_eq!(
        uses.iter().map(|r| r[1].as_deref()).collect::<Vec<_>>(),
        [Some("dept"), Some("emp")]
    );

    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: "main".into(),
        name: "emp".into(),
        object_type: ObjectType::Table,
    }) else {
        panic!()
    };
    assert!(ddl.starts_with("CREATE TABLE emp ("), "{ddl}");
    assert!(
        // As written, SQLite only capitalizing the first words.
        ddl.ends_with(");\n\nCREATE INDEX emp_salary on emp (salary desc);\n"),
        "{ddl}"
    );
    // The DDL runs: the table is made again in another database.
    let copy = TempDb::new("dictionary-copy");
    let mut other = Harness::connect(&copy.create());
    other.ok("create table dept (id integer primary key)");
    for statement in pythia_plsql::split_statements_in(&ddl, Dialect::SQLITE.syntax()) {
        other.ok(statement.text(&ddl));
    }

    let MetaResult::Objects(found) = h.meta(MetaQuery::Find {
        pattern: "e".into(),
        limit: 3,
    }) else {
        panic!()
    };
    assert_eq!(
        found.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(),
        ["emp", "dept", "order"]
    );

    let plan = h.table(MetaQuery::ExplainPlan {
        sql: "select * from emp e join dept d on d.id = e.dept_id where e.salary > :x".into(),
    });
    assert!(!plan.is_empty());
    let objects: Vec<Option<&str>> = plan.iter().map(|r| r[4].as_deref()).collect();
    assert!(
        objects.contains(&Some("e")) && objects.contains(&Some("d")),
        "{plan:?}"
    );

    // An attached database is a schema of its own.
    let attached = TempDb::new("dictionary-attached");
    h.ok(&format!(
        "attach database '{}' as extra",
        attached.create().trim_end_matches("?create")
    ));
    h.ok("create table extra.more (x)");
    let MetaResult::Schemas(schemas) = h.meta(MetaQuery::Schemas) else {
        panic!()
    };
    assert_eq!(schemas, ["main", "extra"]);
    assert_eq!(objects_in(&mut h, "extra"), ["more"]);
}

fn objects_in(h: &mut Harness, schema: &str) -> Vec<String> {
    let MetaResult::Objects(objects) = h.meta(MetaQuery::Objects {
        owner: schema.into(),
        object_type: ObjectType::Table,
        filter: None,
    }) else {
        panic!()
    };
    objects.into_iter().map(|o| o.name).collect()
}

#[test]
fn read_only_and_shared_memory() {
    let db = TempDb::new("readonly");
    let mut h = Harness::connect(&db.create());
    h.ok("create table t (a)");
    let mut reader = Harness::connect(&format!("{}?ro", db.path()));
    assert_eq!(
        reader.scalar("select count(*) from t").as_deref(),
        Some("0")
    );
    assert!(
        reader
            .fails("insert into t values (1)")
            .contains("readonly database"),
    );

    // `:memory:` is one database for every session of the app.
    let mut first = Harness::connect(":memory:");
    first.ok("create table if not exists shared (a)");
    first.ok("delete from shared");
    first.ok("insert into shared values (1)");
    first.session.send(DbRequest::Commit);
    first.settle();
    let mut second = Harness::connect(":memory:");
    assert_eq!(
        second.scalar("select count(*) from shared").as_deref(),
        Some("1")
    );
}

#[test]
fn tools_run_through_the_client() {
    let db = TempDb::new("client");
    let mut h = Harness::connect(&db.create());
    h.ok("create table t (a, b)");
    h.ok("insert into t values (1, 'x'), (2, 'y')");
    h.session.send(DbRequest::Commit);
    h.settle();
    h.session.send(DbRequest::Run {
        id: 5,
        task: Box::new(|client| {
            assert_eq!(client.dialect(), Dialect::SQLITE);
            let rows = client.query("select b from t where a = ?", &["2"])?;
            Ok(Box::new(rows))
        }),
    });
    loop {
        if let DbEvent::TaskDone { id, result } = h.next() {
            assert_eq!(id, 5);
            let rows = result
                .expect("task")
                .downcast::<Vec<Vec<Option<String>>>>()
                .unwrap();
            assert_eq!(*rows, vec![vec![Some("y".to_owned())]]);
            break;
        }
    }
}
