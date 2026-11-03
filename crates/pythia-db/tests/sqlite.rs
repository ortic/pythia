//! SQLite sessions against database files of their own in the temp
//! directory: these run everywhere, no server needed.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use pythia_db::{
    Bind, ConnectParams, DbEvent, DbKind, DbRequest, Detail, Dialect, MetaQuery, MetaResult,
    ObjectType, Script, Session,
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

/// A `file:` URI makes a missing file only when its `mode=rwc` asks for
/// it (#266), as a path does with `?create`: without a `mode=`, SQLite made
/// one.
#[test]
fn a_file_uri_makes_a_missing_file_only_when_asked() {
    // `file:` and the path with `/`, as SQLite reads a URI everywhere.
    let uri = |db: &TempDb, query: &str| {
        let path = db.path().replace('\\', "/").replace(' ', "%20");
        let slash = if path.starts_with('/') { "" } else { "///" };
        format!("file:{slash}{path}{query}")
    };
    let db = TempDb::new("uri");
    for query in ["", "?cache=private", "?mode=rw", "#x"] {
        let error = Harness::try_connect(params(&uri(&db, query)))
            .err()
            .unwrap_or_else(|| panic!("{query}: no file, no connection"));
        assert!(error.contains("There is no database file"), "{error}");
        assert!(error.contains("mode=rwc"), "{error}");
        assert!(!db.0.exists(), "{query}: nothing was made");
    }
    let mut h = Harness::connect(&uri(&db, "?mode=rwc"));
    h.ok("create table t (a)");
    drop(h);
    assert!(db.0.exists());
    // The file there, any URI opens it; `mode=ro` only reads it.
    let mut h = Harness::connect(&uri(&db, ""));
    h.ok("insert into t values (1)");
    assert!(!h.ok("commit"));
    drop(h);
    let mut h = Harness::connect(&uri(&db, "?mode=ro"));
    assert_eq!(h.scalar("select count(*) from t").as_deref(), Some("1"));
    assert!(h.fails("insert into t values (2)").contains("readonly"));
    drop(h);
    // Memory, which no file holds.
    for memory in ["file:r55_memory?mode=memory&cache=shared", "file::memory:"] {
        let mut h = Harness::connect(memory);
        h.ok("create table t (a)");
    }
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

    // Another request closes an open result, and says so; so does a
    // FetchMore of it after.
    h.execute(2, "select value from json_each('[1,2,3]')", 1);
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));
    h.session.send(DbRequest::Rollback);
    assert!(matches!(h.next_result(), DbEvent::QueryClosed { id: 2 }));
    h.session.send(DbRequest::FetchMore {
        id: 2,
        max_rows: 10,
    });
    assert!(matches!(h.next_result(), DbEvent::QueryClosed { id: 2 }));
    assert_eq!(h.scalar("select 'next'").as_deref(), Some("next"));
}

/// A column typed from its first rows is typed again when later ones
/// have blobs (#482): blobs that came after a fetch were shown in hex in a
/// column of text, which exports wrote as text and Edit's check took for
/// a conflict. Whether declared TEXT or without a type.
#[test]
fn blobs_after_the_first_rows_retype_their_column() {
    let db = TempDb::new("retype");
    let mut h = Harness::connect(&db.create());
    h.ok("create table m (id integer primary key, v, t text)");
    h.ok(
        "insert into m values (1, 'one', 'a'), (2, 'two', 'b'), (3, 'three', 'c'), \
          (4, x'00ff', x'cafe'), (5, 'five', 'e')",
    );
    h.execute(1, "select v, t from m order by id", 2);
    let DbEvent::QueryStarted { columns, .. } = h.next_result() else {
        panic!("expected QueryStarted")
    };
    let types = |columns: &[pythia_db::Column]| -> Vec<String> {
        columns.iter().map(|c| c.type_name.clone()).collect()
    };
    assert_eq!(types(&columns), ["TEXT", "TEXT"]);
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));
    h.session.send(DbRequest::FetchMore { id: 1, max_rows: 1 });
    // No blob yet: the types stay.
    let DbEvent::Rows {
        rows, more: true, ..
    } = h.next_result()
    else {
        panic!("expected Rows")
    };
    assert_eq!(rows[0][0].as_deref(), Some("three"));
    h.session.send(DbRequest::FetchMore {
        id: 1,
        max_rows: 10,
    });
    let DbEvent::Retyped { id: 1, columns } = h.next_result() else {
        panic!("expected Retyped")
    };
    assert_eq!(types(&columns), ["BLOB", "BLOB"]);
    let DbEvent::Rows {
        rows, more: false, ..
    } = h.next_result()
    else {
        panic!("expected Rows")
    };
    assert_eq!(rows[0], vec![Some("00FF".into()), Some("CAFE".into())]);
    h.settle();
}

/// A window with rows left to fetch doesn't keep another one from
/// committing (#185): its open statement held a read lock on the file,
/// and the other's commit failed after five seconds with "database is
/// locked". It reads the rest ahead, which ends the statement, and its
/// fetches get the rows as they were.
#[test]
fn rows_left_to_fetch_dont_block_commits() {
    use std::time::Instant;
    let db = TempDb::new("rows-left");
    let mut a = Harness::connect(&db.create());
    a.session.send(DbRequest::SetAutocommit(true));
    a.ok("create table t (n integer)");
    a.ok(
        "insert into t with recursive s(n) as (select 1 union all select n + 1 from s \
           where n < 500) select n from s",
    );
    let mut b = Harness::connect(&db.path());
    a.execute(1, "select n from t order by n", 10);
    assert!(matches!(a.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(a.next_result(), DbEvent::Rows { more: true, .. }));
    let started = Instant::now();
    b.ok("insert into t values (501)");
    b.session.send(DbRequest::Commit);
    let mut committed = false;
    loop {
        match b.next() {
            DbEvent::Notice(n) if n == "Committed" => committed = true,
            DbEvent::Error(e) => panic!("{e}"),
            DbEvent::Transaction { .. } if committed => break,
            _ => {}
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    a.session.send(DbRequest::FetchMore {
        id: 1,
        max_rows: 1000,
    });
    let DbEvent::Rows { rows, more, .. } = a.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!((rows.len(), more), (490, false));
    assert_eq!(rows[489][0].as_deref(), Some("500"));
    a.settle();
    assert_eq!(a.scalar("select count(*) from t").as_deref(), Some("501"));
}

/// Text that isn't UTF-8 shows as Windows-1252, not with U+FFFD for its
/// other bytes (#413); compared with that text, as Edit checks a row, it
/// isn't found: its bytes differ.
#[test]
fn text_that_isnt_utf8() {
    let db = TempDb::new("latin");
    let mut h = Harness::connect(&db.create());
    h.ok("create table t (s text)");
    h.ok("insert into t values (cast(X'4DFC6C6C657220803520' as text))");
    assert_eq!(h.scalar("select s from t").as_deref(), Some("Müller €5 "));
    assert_eq!(
        h.scalar("select count(*) from t where s = 'Müller €5 '")
            .as_deref(),
        Some("0")
    );
}

/// For the grid a blob is cut short with its size; for a file it comes
/// whole, and a batch ends once it holds 64 MB of text, so rows of large
/// blobs come a few at a time (#204).
#[test]
fn whole_values_for_a_file() {
    let db = TempDb::new("whole");
    let mut h = Harness::connect(&db.create());
    h.ok("create table b (id integer primary key, v blob)");
    h.ok(
        "insert into b with recursive n(n) as (select 1 union all select n + 1 from n \
         where n < 40) select n, randomblob(1048576) from n",
    );
    let shown = h.scalar("select v from b where id = 1").unwrap();
    assert_eq!(pythia_db::cut_size(&shown), Some(1048576));
    assert_eq!(
        shown.len(),
        2 * pythia_db::SHOWN_BYTES + "… (1048576 bytes)".len()
    );

    h.session.send(DbRequest::ExecuteWhole {
        id: 1,
        sql: "select id, v from b order by id".into(),
        max_rows: 1000,
    });
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    let DbEvent::Rows { rows, more, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    // 64 MB of hex, and the row that goes past it.
    assert!(
        more && rows.len() == 32,
        "{} rows, more: {more}",
        rows.len()
    );
    let mut fetched = rows;
    h.session.send(DbRequest::FetchMore {
        id: 1,
        max_rows: 1000,
    });
    let DbEvent::Rows { rows, more, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!((rows.len(), more), (8, false));
    fetched.extend(rows);
    for (i, row) in fetched.iter().enumerate() {
        assert_eq!(row[0], Some((i + 1).to_string()));
        assert_eq!(
            row[1].as_ref().map(String::len),
            Some(2 * 1048576),
            "row {i}"
        );
    }
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
        binds: vec![
            ("A".into(), Bind::Text(Some("7".into()))),
            ("b".into(), Bind::Text(None)),
        ],
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

/// A script's statements, sent at once, run one after the other (#291):
/// after one that fails the session skips the rest, unless the script goes
/// on after errors; a stopped script, or one whose session's handle is
/// gone, runs no more of them, while a Commit sent before still runs.
#[test]
fn script_statements_stop_at_an_error() {
    let db = TempDb::new("script");
    let mut h = Harness::connect(&db.create());
    h.ok("create table r51_t (id integer primary key)");
    let send = |h: &mut Harness, script: &Script, ids: std::ops::Range<u64>| {
        for id in ids {
            let sql = format!("insert into r51_t values ({})", id % 10);
            let request = DbRequest::Execute {
                id,
                sql,
                max_rows: 10,
                binds: vec![],
            };
            h.session.send_in(request, script);
        }
    };
    // The answers until `last`, or until nothing comes for a while.
    let answered = |h: &mut Harness, last: u64| {
        let mut ids = Vec::new();
        while !ids.contains(&last) {
            let Some(event) = (loop {
                if let Some(event) = h.session.try_recv() {
                    break Some(event);
                }
                if h.wake.recv_timeout(Duration::from_millis(500)).is_err() {
                    break None;
                }
            }) else {
                break;
            };
            match event {
                DbEvent::Executed { id, .. } => ids.push(id),
                DbEvent::Failed { id, .. } => ids.push(id + 100),
                _ => {}
            }
        }
        ids
    };
    // 11 fails as a duplicate of 1 (Failed as 111).
    let script = Script::new();
    send(&mut h, &script, 1..4);
    send(&mut h, &script, 11..14);
    assert_eq!(answered(&mut h, 13), [1, 2, 3, 111]);
    assert!(script.is_stopped());
    assert_eq!(h.scalar("select count(*) from r51_t").as_deref(), Some("3"));
    h.ok("rollback");

    let script = Script::new();
    script.continue_on_error(true);
    send(&mut h, &script, 1..4);
    send(&mut h, &script, 11..14);
    assert_eq!(answered(&mut h, 113), [1, 2, 3, 111, 112, 113]);
    h.settle();
    assert!(!script.is_stopped());
    h.ok("rollback");

    let script = Script::new();
    script.stop();
    send(&mut h, &script, 1..3);
    assert_eq!(answered(&mut h, 2), Vec::<u64>::new());

    // The handle dropped while a statement runs: the rest is skipped, what
    // was sent outside the script before isn't.
    let script = Script::new();
    let execute = |id: u64, sql: &str| DbRequest::Execute {
        id,
        sql: sql.into(),
        max_rows: 10,
        binds: vec![],
    };
    h.session.send_in(
        execute(
            20,
            "with recursive r (n) as (select 1 union all select n + 1 from r \
             where n < 10000000) select count(*) from r",
        ),
        &script,
    );
    h.session
        .send_in(execute(21, "insert into r51_t values (7)"), &script);
    h.session.send(execute(22, "insert into r51_t values (8)"));
    h.session.send(DbRequest::Commit);
    // Counting, which says nothing until it's done.
    std::thread::sleep(Duration::from_millis(200));
    drop(h);
    let mut h = Harness::connect(&db.path());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while h.scalar("select count(*) from r51_t").as_deref() != Some("1") {
        assert!(std::time::Instant::now() < deadline, "not committed");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        h.scalar("select group_concat(id) from r51_t").as_deref(),
        Some("8")
    );
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

/// SQLite keeps a statement as written, a line comment at its end too: the
/// DDL's `;` goes on a line of its own after it (#206).
#[test]
fn ddl_ends_after_a_line_comment() {
    let db = TempDb::new("comments");
    let mut h = Harness::connect(&db.create());
    for sql in [
        "create table fs_t (a, b)",
        "create index fs_t_a on fs_t (a) -- first index;",
        "create index fs_t_b on fs_t (b)",
        "create view fs_v as select 'foo' as a\nwhere 1\n    -- and 0",
    ] {
        h.ok(sql);
    }
    let ddl = |h: &mut Harness, name: &str, object_type| {
        let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
            owner: "main".into(),
            name: name.into(),
            object_type,
        }) else {
            panic!()
        };
        ddl
    };
    let table = ddl(&mut h, "fs_t", ObjectType::Table);
    let view = ddl(&mut h, "fs_v", ObjectType::View);
    assert!(view.ends_with("-- and 0\n;\n"), "{view}");
    // Run again, elsewhere: the table with both indexes, and the view.
    let copy = TempDb::new("comments-copy");
    let mut other = Harness::connect(&copy.create());
    let script = format!("{table}{view}select 1;");
    let statements = pythia_plsql::split_statements_in(&script, Dialect::SQLITE.syntax());
    assert_eq!(statements.len(), 5, "{script}");
    for statement in &statements {
        other.ok(statement.text(&script));
    }
    assert_eq!(
        other
            .scalar("select group_concat(name, ',') from sqlite_master where name like 'fs_%'")
            .as_deref(),
        Some("fs_t,fs_t_a,fs_t_b,fs_v")
    );
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

/// Sessions of a database in a shared cache, as `:memory:` is, wait for
/// each other's locks as sessions of a file do (#374): up to five seconds,
/// then "Another window holds …". SQLite fails at once on a shared cache's
/// table and schema locks (SQLITE_LOCKED): another session's SELECT and
/// INSERT of a table with an uncommitted insert failed with "database table
/// is locked", and while a session had uncommitted DDL every lookup failed
/// with "database schema is locked", so the browser and completion came up
/// empty, and a new window didn't connect. Lookups now answer as they did
/// before the DDL, and wait for one not made before. Break ends a wait.
#[test]
fn shared_cache_sessions_wait_for_each_others_locks() {
    use std::time::Instant;
    let memory = "file:r85_locks?mode=memory&cache=shared";
    let mut a = Harness::connect(memory);
    let mut b = Harness::connect(memory);
    let mut main = Harness::connect(memory);
    a.ok("create table t (id integer primary key)");
    a.ok("insert into t values (1)");
    a.session.send(DbRequest::Commit);
    a.settle();
    let tables = |h: &mut Harness| objects_in(h, "main");
    assert_eq!(tables(&mut main), ["t"]);
    // A while later, in `h`'s session thread, ends `a`'s transaction.
    let later = |a: &mut Harness, commit: bool| {
        std::thread::sleep(Duration::from_millis(300));
        a.session.send(if commit {
            DbRequest::Commit
        } else {
            DbRequest::Rollback
        });
        a.settle();
    };

    // An uncommitted insert: another session's SELECT and INSERT wait for
    // it.
    assert!(a.ok("insert into t values (2)"));
    let started = Instant::now();
    b.execute(1, "select count(*) from t", 10);
    later(&mut a, true);
    let event = b.next_result();
    assert!(
        matches!(event, DbEvent::QueryStarted { .. }),
        "the SELECT: {event:?}"
    );
    let DbEvent::Rows { rows, .. } = b.next_result() else {
        panic!("rows")
    };
    b.settle();
    assert_eq!(rows[0][0].as_deref(), Some("2"));
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(a.ok("insert into t values (3)"));
    b.execute(2, "insert into t values (4)", 10);
    later(&mut a, false);
    let event = b.next_result();
    assert!(
        matches!(event, DbEvent::Executed { .. }),
        "the INSERT: {event:?}"
    );
    b.settle();
    b.session.send(DbRequest::Commit);
    b.settle();
    // Five seconds, then what holds it.
    assert!(a.ok("insert into t values (5)"));
    let started = Instant::now();
    let error = b.fails("insert into t values (6)");
    let waited = started.elapsed();
    assert!(
        error.contains("database table is locked") && error.contains("Another window holds"),
        "{error}"
    );
    assert!(waited >= Duration::from_secs(5), "{waited:?}");
    // Break ends the wait.
    let started = Instant::now();
    b.execute(3, "insert into t values (6)", 10);
    std::thread::sleep(Duration::from_millis(300));
    b.session.cancel();
    let DbEvent::Failed { error, .. } = b.next_result() else {
        panic!("cancelled")
    };
    b.settle();
    assert_eq!(error.text(), "Cancelled");
    assert!(started.elapsed() < Duration::from_secs(4));

    // Uncommitted DDL: a lookup made before answers as it did then, also
    // in a task (completion's); one not made before, and a new session,
    // wait for it.
    assert!(a.ok("create table u (x)"));
    assert_eq!(tables(&mut main), ["t"]);
    main.session.send(DbRequest::Run {
        id: 7,
        task: Box::new(|client| {
            let found = client.meta(&MetaQuery::Objects {
                owner: "main".into(),
                object_type: ObjectType::Table,
                filter: None,
            })?;
            let MetaResult::Objects(objects) = found else {
                panic!("objects")
            };
            let names: Vec<String> = objects.into_iter().map(|o| o.name).collect();
            Ok(Box::new(names))
        }),
    });
    loop {
        if let DbEvent::TaskDone { result, .. } = main.next() {
            let names = result.expect("the task").downcast::<Vec<String>>().unwrap();
            assert_eq!(*names, ["t"]);
            break;
        }
    }
    main.session.send(DbRequest::Meta {
        id: 8,
        query: MetaQuery::Columns {
            owner: "main".into(),
            table: "t".into(),
            keys: false,
        },
    });
    let (tx, wake) = mpsc::channel();
    let session = Session::connect(params(memory), move || {
        let _ = tx.send(());
    });
    let mut c = Harness { session, wake };
    later(&mut a, false);
    loop {
        if let DbEvent::Meta { result, .. } = main.next() {
            let Ok(MetaResult::Columns(columns)) = result else {
                panic!("the columns: {result:?}")
            };
            assert_eq!(columns[0].name, "id");
            break;
        }
    }
    let event = c.next();
    assert!(
        matches!(event, DbEvent::Connected(_)),
        "a new session: {event:?}"
    );
    a.ok("drop table t");
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

/// Object filters and Find find names without case whatever their letters:
/// SQLite's upper() and LIKE change ASCII letters only.
#[test]
fn filters_match_any_letters_without_case() {
    let db = TempDb::new("letters");
    let mut h = Harness::connect(&db.create());
    for sql in [
        "create table \"r62_čaj\" (x)",
        "create table \"r62_bière\" (x)",
        "create table \"r62_ÖL\" (x)",
        "create table r62_tea (x)",
        "create view \"r62_Straße\" as select 1",
    ] {
        h.ok(sql);
    }
    let filtered = |h: &mut Harness, object_type, filter: &str| -> Vec<String> {
        let MetaResult::Objects(objects) = h.meta(MetaQuery::Objects {
            owner: "main".into(),
            object_type,
            filter: Some(filter.into()),
        }) else {
            panic!("expected objects")
        };
        objects.into_iter().map(|o| o.name).collect()
    };
    let mut tables = |filter: &str| filtered(&mut h, ObjectType::Table, filter);
    assert_eq!(tables("čaj"), ["r62_čaj"]);
    assert_eq!(tables("ČAJ"), ["r62_čaj"]);
    assert_eq!(tables("r62_Č"), ["r62_čaj"]);
    assert_eq!(tables("BIÈRE"), ["r62_bière"]);
    assert_eq!(tables("öl"), ["r62_ÖL"]);
    assert_eq!(tables("R62_?A"), ["r62_čaj"]);
    assert_eq!(tables("r62_?EA"), ["r62_tea"]);
    assert_eq!(tables("r62*è"), ["r62_bière"]);
    assert_eq!(tables("r62_t*a"), ["r62_tea"]);
    assert_eq!(tables("r62%"), Vec::<String>::new());
    assert_eq!(tables("r62.t"), Vec::<String>::new());
    assert_eq!(
        tables("R62_"),
        ["r62_bière", "r62_tea", "r62_ÖL", "r62_čaj"]
    );
    assert_eq!(filtered(&mut h, ObjectType::View, "straße"), ["r62_Straße"]);
    assert_eq!(filtered(&mut h, ObjectType::View, "STRAßE"), ["r62_Straße"]);
    let mut found = |pattern: &str| -> Vec<String> {
        let MetaResult::Objects(found) = h.meta(MetaQuery::Find {
            pattern: pattern.into(),
            limit: 10,
        }) else {
            panic!("expected objects")
        };
        found.into_iter().map(|o| o.name).collect()
    };
    assert_eq!(found("R62_ČA"), ["r62_čaj"]);
    assert_eq!(found("ère"), ["r62_bière"]);
    assert_eq!(found("r62_*e"), ["r62_tea", "r62_bière", "r62_Straße"]);
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

/// Bytes bind as a blob and text as text, a number too, whole: megabytes
/// into a BLOB and a TEXT column, inserted and updated (#293).
#[test]
fn values_bound_whole() {
    let db = TempDb::new("bound");
    let mut h = Harness::connect(&db.create());
    h.ok("create table lob_bound (id integer primary key, b blob, t text)");
    let (first, second) = (whole_values(0), whole_values(1));
    let insert = "insert into lob_bound (id, b, t) values (1, :b, :t)";
    assert_eq!(h.bound(insert, binds(&first, &["b", "t"])), Ok(Some(1)));
    let update = "update lob_bound set b = :b, t = :t where id = 1";
    assert_eq!(h.bound(update, binds(&second, &["b", "t"])), Ok(Some(1)));
    let row = h.whole_row("select b, t from lob_bound");
    assert!(unhex(row[0].as_deref().unwrap()) == second.0, "b");
    assert!(row[1].as_deref() == Some(second.1.as_str()), "t");
}
