//! Editing results against the databases: the key comes from the table the
//! query read, and every value of every type is found again by the check an
//! edit makes and written back the way it was shown. PostgreSQL and SQL
//! Server are skipped unless `PYTHIA_TEST_POSTGRES` or `PYTHIA_TEST_MSSQL`
//! is set (`user/password@host:port/db`); SQLite runs on a file of its own.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

use pythia_db::{
    Cell, Client, Column, ConnectParams, DbError, DbEvent, DbKind, DbRequest, Dialect, Session,
};
use pythia_ui::{Changes, Loaded, RowRef};

use super::{Editable, RowKey, dml, edit_query, has_key_columns, row_key, same};

struct Live {
    session: Session,
    wake: mpsc::Receiver<()>,
    dialect: Dialect,
}

/// What a statement gave.
#[derive(Debug)]
enum Ran {
    Rows(Vec<Column>, Vec<Vec<Cell>>),
    Done(Option<u64>),
}

impl Live {
    fn connect(env: &str, kind: DbKind) -> Option<Self> {
        let spec = std::env::var(env).ok()?;
        let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
        let (username, password) = credentials.split_once('/').unwrap();
        Some(Self::open(env, kind, username, password, connect_string))
    }

    /// A new SQLite database in the temp directory (removed first).
    fn sqlite(name: &str) -> (Self, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("pythia-live-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let connect_string = format!("{}?create", path.display());
        let live = Self::open(name, DbKind::Sqlite, "", "", &connect_string);
        (live, path)
    }

    fn open(env: &str, kind: DbKind, username: &str, password: &str, connect_string: &str) -> Self {
        let (tx, wake) = mpsc::channel();
        let session = Session::connect(
            ConnectParams {
                kind,
                username: username.into(),
                password: password.into(),
                connect_string: connect_string.into(),
                privilege: Default::default(),
                init_sql: Vec::new(),
                ssh: None,
            },
            move || {
                let _ = tx.send(());
            },
        );
        let mut live = Self {
            session,
            wake,
            dialect: Dialect(kind),
        };
        match live.next() {
            DbEvent::Connected(_) => {}
            DbEvent::ConnectFailed(e) => panic!("{env}: {e}"),
            _ => panic!("{env}: not connected"),
        }
        live.session.send(DbRequest::SetAutocommit(true));
        live
    }

    fn next(&mut self) -> DbEvent {
        loop {
            if let Some(event) = self.session.try_recv() {
                return event;
            }
            self.wake
                .recv_timeout(Duration::from_secs(60))
                .expect("timed out");
        }
    }

    /// Runs a statement as a SQL window does.
    fn run(&mut self, sql: &str) -> Result<Ran, DbError> {
        self.run_with(sql, Vec::new())
    }

    /// Runs a statement of an Apply, with its binds (#293).
    fn apply(&mut self, statement: &super::Dml) -> Result<Ran, DbError> {
        self.run_with(&statement.sql, statement.binds.clone())
    }

    fn run_with(
        &mut self,
        sql: &str,
        binds: Vec<(String, pythia_db::Bind)>,
    ) -> Result<Ran, DbError> {
        self.session.send(DbRequest::Execute {
            id: 1,
            sql: sql.into(),
            max_rows: 100_000,
            binds,
        });
        let mut columns = Vec::new();
        let mut outcome = None;
        // The session's transaction is reported last; Oracle's before the
        // event that ends the statement.
        let oracle = self.dialect.kind() == DbKind::Oracle;
        loop {
            match self.next() {
                DbEvent::QueryStarted { columns: c, .. } => columns = c,
                DbEvent::Rows { rows, .. } => {
                    outcome = Some(Ok(Ran::Rows(std::mem::take(&mut columns), rows)));
                }
                DbEvent::Executed { summary, .. } => {
                    outcome = Some(Ok(Ran::Done(summary.rows_affected)));
                }
                DbEvent::Failed { error, .. } => outcome = Some(Err(error)),
                DbEvent::Transaction { .. } if !oracle => {
                    if let Some(outcome) = outcome {
                        return outcome;
                    }
                }
                _ => {}
            }
            if oracle && let Some(outcome) = outcome.take() {
                return outcome;
            }
        }
    }

    fn ok(&mut self, sql: &str) -> Ran {
        self.run(sql).unwrap_or_else(|e| panic!("{e}\n{sql}"))
    }

    /// Runs statements, one by one, that must succeed.
    fn script(&mut self, statements: &[&str]) {
        for sql in statements {
            self.ok(sql);
        }
    }

    fn query(&mut self, sql: &str) -> (Vec<Column>, Vec<Vec<Cell>>) {
        match self.ok(sql) {
            Ran::Rows(columns, rows) => (columns, rows),
            other => panic!("{sql}: {other:?}"),
        }
    }

    fn count(&mut self, sql: &str) -> String {
        let (_, rows) = self.query(sql);
        rows[0][0].clone().unwrap_or_default()
    }

    /// The key `row_key` finds for `table`, on the session as the window's
    /// own lookup runs.
    fn key(&mut self, table: &str) -> Vec<String> {
        let table = table.to_owned();
        self.session.send(DbRequest::Run {
            id: 2,
            task: Box::new(move |client: &mut dyn Client| {
                row_key(client, &table).map(|key| Box::new(key) as Box<dyn Any + Send>)
            }),
        });
        loop {
            if let DbEvent::TaskDone { result, .. } = self.next() {
                let any = result.unwrap_or_else(|e| panic!("{e}"));
                return *any.downcast::<Vec<String>>().expect("a key");
            }
        }
    }

    /// The result of `table` as *Edit* fetches it, with its key.
    fn editable(&mut self, table: &str, order: &str) -> (Editable, Vec<Column>, Vec<Vec<Cell>>) {
        let key = self.key(table);
        assert!(!key.is_empty(), "{table} has no key");
        let edit = Editable {
            table: table.into(),
            source: format!("select * from {table} order by {order}"),
            key: RowKey::Columns(key),
        };
        let sql = edit_query(self.dialect, &edit).expect("an editable query");
        let (columns, rows) = self.query(&sql);
        (edit, columns, rows)
    }

    /// Sets every editable value of `table` to itself, as if each cell had
    /// been typed in again as shown: each UPDATE must find its row by the
    /// key and the value fetched, and the values must read back the same.
    /// Returns the columns left out of the check.
    fn rewrite_all(&mut self, table: &str, order: &str, skip: &[&str]) -> Vec<String> {
        let (edit, columns, rows) = self.editable(table, order);
        let key_start = columns.len() - edit.key.width();
        let mut unchecked = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, col) in columns[..key_start].iter().enumerate() {
                if skip.contains(&col.name.as_str()) {
                    continue;
                }
                if same(self.dialect, &col.name, col, row[c].as_deref()).is_none() {
                    unchecked.push(col.name.clone());
                }
                let changes = Changes {
                    updates: BTreeMap::from([(r, BTreeMap::from([(c, row[c].clone())]))]),
                    ..Changes::default()
                };
                for statement in dml(self.dialect, &edit, &columns, &rows, &changes) {
                    let ran = self.ok(&statement.sql);
                    assert!(
                        matches!(ran, Ran::Done(Some(1))),
                        "{} ({:?}) didn't find its row: {ran:?}\n{}",
                        col.name,
                        row[c],
                        statement.sql
                    );
                }
            }
        }
        let (_, _, again) = self.editable(table, order);
        for (r, (before, after)) in rows.iter().zip(&again).enumerate() {
            for (c, col) in columns.iter().enumerate() {
                assert_eq!(before[c], after[c], "row {r}, {}", col.name);
            }
        }
        unchecked.sort();
        unchecked.dedup();
        unchecked
    }

    /// Whether the check `same` makes for each value of `table` finds its
    /// row (without writing anything).
    fn checks_find_rows(&mut self, table: &str, order: &str) {
        let (edit, columns, rows) = self.editable(table, order);
        let key_start = columns.len() - edit.key.width();
        let RowKey::Columns(key) = &edit.key else {
            unreachable!()
        };
        for row in &rows {
            let found: Vec<String> = key
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let col = &columns[key_start + i];
                    same(self.dialect, k, col, row[key_start + i].as_deref()).unwrap()
                })
                .collect();
            for (c, col) in columns[..key_start].iter().enumerate() {
                let Some(check) = same(self.dialect, &col.name, col, row[c].as_deref()) else {
                    continue;
                };
                let sql = format!(
                    "select count(*) from {table} where {} and {check}",
                    found.join(" and ")
                );
                assert_eq!(self.count(&sql), "1", "{sql}");
            }
        }
    }
}

/// Random finite, normal doubles and reals (xorshift: the same each run),
/// half of them between 2^-30 and 2^30, which are shown without an
/// exponent and so go back as decimals.
fn random_floats(n: usize) -> Vec<(f64, f32)> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut values = Vec::new();
    while values.len() < n {
        let (mut d, mut r) = (next(), next() as u32);
        if values.len() % 2 == 0 {
            d = (d & 0x800f_ffff_ffff_ffff) | ((1023 - 30 + next() % 60) << 52);
            r = (r & 0x807f_ffff) | ((127 - 30 + (next() % 60) as u32) << 23);
        }
        let (d, r) = (f64::from_bits(d), f32::from_bits(r));
        if d.is_normal() && r.is_normal() {
            values.push((d, r));
        }
    }
    // And some that are shown in many digits, or round.
    values.extend([
        (0.1 + 0.2, 0.1),
        (1e300, 3.402_823_5e38),
        (-2.225_073_858_507_201_4e-308, 1.175_494_4e-38),
        (123_456.789, 123_456.79),
        (1.0, -1.0),
    ]);
    values
}

/// MySQL: the key of the table the query read, also a temporary table,
/// which information_schema doesn't list and which hides a table of its
/// name.
#[test]
fn mysql_keys() {
    let Some(mut db) = Live::connect("PYTHIA_TEST_MYSQL", DbKind::MySql) else {
        return;
    };
    db.script(&[
        "drop view if exists pythia_live_v",
        "drop table if exists pythia_live_k, pythia_live_u, pythia_live_n, pythia_live_t",
        "create table pythia_live_k (id int primary key, code varchar(10) not null unique)",
        "create view pythia_live_v as select * from pythia_live_k",
        "create table pythia_live_u (n int, e varchar(10) not null, x int not null, \
           unique key pythia_live_u_a (n), unique key pythia_live_u_c (x))",
        "create table pythia_live_n (x int)",
        "create table pythia_live_t (id int primary key)",
        "create temporary table pythia_live_t (t int not null, u varchar(5) not null, \
           unique key (u), primary key (t))",
        "create temporary table pythia_live_tn (x int)",
    ]);
    // A functional key part (MySQL 8.0.13; MariaDB has none).
    if !db.count("select version()").contains("MariaDB") {
        db.ok("alter table pythia_live_u add unique key pythia_live_u_b ((lower(e)))");
    }
    assert_eq!(db.key("pythia_live_k"), ["id"]);
    assert_eq!(db.key("pythia.pythia_live_k"), ["id"]);
    assert_eq!(db.key("`pythia_live_k`"), ["id"]);
    // Not on a NULL or an expression.
    assert_eq!(db.key("pythia_live_u"), ["x"]);
    // The temporary table, not the table it hides.
    assert_eq!(db.key("pythia_live_t"), ["t"]);
    assert!(db.key("pythia_live_tn").is_empty());
    assert!(db.key("pythia_live_v").is_empty());
    assert!(db.key("pythia_live_n").is_empty());
    assert!(db.key("pythia_live_missing").is_empty());
    // A `#` comment after the select list: the key columns come before it,
    // where the server reads them.
    let edit = Editable {
        table: "pythia_live_k".into(),
        source: "select id, code # the columns\nfrom pythia_live_k".into(),
        key: RowKey::Columns(vec!["id".into()]),
    };
    let sql = edit_query(db.dialect, &edit).expect("an editable query");
    let (columns, _) = db.query(&sql);
    assert!(has_key_columns(&edit, &columns), "{sql}");
    db.script(&[
        "drop temporary table pythia_live_t, pythia_live_tn",
        "drop view pythia_live_v",
        "drop table pythia_live_k, pythia_live_u, pythia_live_n, pythia_live_t",
    ]);
}

#[test]
fn postgres_keys() {
    let Some(mut db) = Live::connect("PYTHIA_TEST_POSTGRES", DbKind::Postgres) else {
        return;
    };
    db.script(&[
        "drop schema if exists pythia_live_s cascade",
        "drop view if exists pythia_live_v",
        "drop table if exists pythia_live_k, \"Pythia_Live_K\", pythia_live_u, pythia_live_n",
        "create schema pythia_live_s",
        "create table pythia_live_k (id int primary key, code text not null unique)",
        "create table pythia_live_s.pythia_live_k (n int unique, a int not null, \
           b text not null, unique (a, b))",
        "create table \"Pythia_Live_K\" (code text primary key)",
        "create view pythia_live_v as select * from pythia_live_k",
        "create table pythia_live_u (n int, e text not null, i int not null, x int not null)",
        "create unique index pythia_live_u_a on pythia_live_u (n)",
        "create unique index pythia_live_u_b on pythia_live_u (lower(e))",
        "create unique index pythia_live_u_c on pythia_live_u (i) where i > 0",
        "create unique index pythia_live_u_d on pythia_live_u (x) include (n)",
        "create table pythia_live_n (x int)",
    ]);
    // The primary key comes first; unquoted names fold to lowercase.
    assert_eq!(db.key("pythia_live_k"), ["id"]);
    assert_eq!(db.key("PYTHIA_LIVE_K"), ["id"]);
    assert_eq!(db.key("public.pythia_live_k"), ["id"]);
    assert_eq!(db.key("pythia.public.pythia_live_k"), ["id"]);
    // The table of the same name in another schema has a key of its own.
    assert_eq!(db.key("pythia_live_s.pythia_live_k"), ["a", "b"]);
    assert_eq!(db.key("\"Pythia_Live_K\""), ["code"]);
    // The search path decides, as it did for the query.
    db.ok("set search_path = pythia_live_s, public");
    assert_eq!(db.key("pythia_live_k"), ["a", "b"]);
    db.ok("set search_path = public");
    // Not on a NULL, an expression or some rows; INCLUDE isn't the key.
    assert_eq!(db.key("pythia_live_u"), ["x"]);
    // A view has no key, a table may have none, a name may be no table.
    assert!(db.key("pythia_live_v").is_empty());
    assert!(db.key("pythia_live_n").is_empty());
    assert!(db.key("pythia_live_missing").is_empty());
    assert!(db.key("generate_series").is_empty());
    db.script(&[
        "drop schema pythia_live_s cascade",
        "drop view pythia_live_v",
        "drop table pythia_live_k, \"Pythia_Live_K\", pythia_live_u, pythia_live_n",
    ]);
}

#[test]
fn postgres_values_find_their_rows() {
    let Some(mut db) = Live::connect("PYTHIA_TEST_POSTGRES", DbKind::Postgres) else {
        return;
    };
    db.script(&[
        "drop table if exists pythia_live_types, pythia_live_floats",
        "drop type if exists pythia_live_mood",
        "create type pythia_live_mood as enum ('sad', 'happy')",
        "create table pythia_live_types (id integer primary key, si smallint, i integer, \
           bi bigint, nu numeric(20,3), r real, d double precision, mo money, b boolean, \
           tx text, vc varchar(30), c char(5), nm name, by bytea, da date, ti time, ttz timetz, \
           ts timestamp, tstz timestamptz, iv interval, u uuid, ip inet, cid cidr, \
           mac macaddr, bt bit(3), vbt varbit(5), j json, jb jsonb, x xml, ia integer[], \
           ta text[], ra real[], pt point, bx box, tv tsvector, mood pythia_live_mood, \
           rng int4range)",
        "insert into pythia_live_types values (1, -32768, 2147483647, -9223372036854775808, \
           12345678901234567.125, 0.1, 0.1::float8 + 0.2, 12.34, true, \
           E'C:\\\\x ''q'' \\x26 \u{e9}\\nnew line', 'it''s', 'ab', 'pg_class', '\\x00ff', \
           '2026-01-02', '12:34:56.123456', '12:34:56+05:30', '2026-01-02 03:04:05.123456', \
           '2026-01-02 03:04:05.123456+01', '1 day 02:03:04.5', \
           '6f9619ff-8b86-d011-b42d-00c04fc964ff', '192.168.0.1', '10.0.0.0/8', \
           '08:00:2b:01:02:03', B'101', B'11', '{\"a\":  [1, 2.50]}', '{\"b\": [1, 2.50]}', \
           '<a b=\"1\">x &amp; y</a>', '{1,NULL,3}', '{\"x y\",z}', '{0.1,1e-30}', \
           '(1.5,2)', '((1,1),(0,0))', 'a fat cat', 'happy', '[1,5)')",
        "insert into pythia_live_types (id) values (2)",
        "create table pythia_live_floats (id integer primary key, d double precision, r real)",
    ]);
    let floats = random_floats(200);
    for chunk in floats.chunks(50) {
        let values: Vec<String> = chunk
            .iter()
            .map(|(d, r)| format!("('{d:e}', '{r:e}')"))
            .collect();
        db.ok(&format!(
            "insert into pythia_live_floats select row_number() over () + \
               (select count(*) from pythia_live_floats), d::float8, r::real \
               from (values {}) v(d, r)",
            values.join(", ")
        ));
    }
    // Every type can be checked: json, xml, points, floats and arrays as
    // their text.
    assert!(db.rewrite_all("pythia_live_types", "id", &[]).is_empty());
    assert!(db.rewrite_all("pythia_live_floats", "id", &[]).is_empty());
    // Floats shown rounded are found as shown.
    db.ok("set extra_float_digits = 0");
    db.checks_find_rows("pythia_live_floats", "id");
    db.checks_find_rows("pythia_live_types", "id");
    db.ok("set extra_float_digits = 1");
    db.script(&[
        "drop table pythia_live_types, pythia_live_floats",
        "drop type pythia_live_mood",
    ]);
}

#[test]
fn sqlserver_keys() {
    let Some(mut db) = Live::connect("PYTHIA_TEST_MSSQL", DbKind::SqlServer) else {
        return;
    };
    db.script(&[
        "drop view if exists dbo.pythia_live_v",
        "drop table if exists dbo.pythia_live_k, pythia_live_s.pythia_live_k, \
           [Pythia Live K], pythia_live_u, pythia_live_n",
        "drop schema if exists pythia_live_s",
        "create schema pythia_live_s",
        "create table dbo.pythia_live_k (id int primary key, code nvarchar(10) not null unique)",
        "create table pythia_live_s.pythia_live_k (n int null unique, a int not null, \
           b varchar(5) not null, constraint pythia_live_ab unique (a, b))",
        "create table [Pythia Live K] ([the code] nvarchar(10) primary key)",
        "create view dbo.pythia_live_v with schemabinding as \
           select id, code from dbo.pythia_live_k",
        "create unique clustered index pythia_live_vi on dbo.pythia_live_v (id)",
        "create table pythia_live_u (n int null, i int not null, v sql_variant not null, \
           x int not null)",
        "create unique index pythia_live_u_a on pythia_live_u (n)",
        "create unique index pythia_live_u_b on pythia_live_u (i) where i > 0",
        "create unique index pythia_live_u_c on pythia_live_u (v)",
        "create unique index pythia_live_u_d on pythia_live_u (x) include (n)",
        "create table pythia_live_n (x int)",
        "create table #pythia_live_t (t int not null primary key, u int not null unique)",
        "create table ##pythia_live_g (g int not null unique)",
    ]);
    // A temporary table, local or global, is found in tempdb.
    assert_eq!(db.key("#pythia_live_t"), ["t"]);
    assert_eq!(db.key("[#pythia_live_t]"), ["t"]);
    assert_eq!(db.key("##pythia_live_g"), ["g"]);
    assert!(db.key("#pythia_live_missing").is_empty());
    db.ok("drop table #pythia_live_t, ##pythia_live_g");
    // The primary key comes first; the default schema and the collation
    // find the table as the query did.
    assert_eq!(db.key("pythia_live_k"), ["id"]);
    assert_eq!(db.key("PYTHIA_LIVE_K"), ["id"]);
    assert_eq!(db.key("[dbo].[pythia_live_k]"), ["id"]);
    assert_eq!(db.key("pythia.dbo.pythia_live_k"), ["id"]);
    assert_eq!(db.key("pythia..pythia_live_k"), ["id"]);
    assert_eq!(db.key("master.dbo.pythia_live_k"), Vec::<String>::new());
    // The table of the same name in another schema has a key of its own.
    assert_eq!(db.key("pythia_live_s.pythia_live_k"), ["a", "b"]);
    assert_eq!(db.key("[Pythia Live K]"), ["the code"]);
    // Not on a NULL, some rows or a sql_variant; INCLUDE isn't the key.
    assert_eq!(db.key("pythia_live_u"), ["x"]);
    // An indexed view's unique index isn't a table's key.
    assert!(db.key("pythia_live_v").is_empty());
    assert!(db.key("pythia_live_n").is_empty());
    assert!(db.key("pythia_live_missing").is_empty());
    db.script(&[
        "drop view dbo.pythia_live_v",
        "drop table dbo.pythia_live_k, pythia_live_s.pythia_live_k, [Pythia Live K], \
           pythia_live_u, pythia_live_n",
        "drop schema pythia_live_s",
    ]);
}

#[test]
fn sqlserver_values_find_their_rows() {
    let Some(mut db) = Live::connect("PYTHIA_TEST_MSSQL", DbKind::SqlServer) else {
        return;
    };
    db.script(&[
        "drop table if exists pythia_live_types, pythia_live_floats, pythia_live_h",
        "create table pythia_live_types (id int primary key, ti tinyint, si smallint, i int, \
           bi bigint, de decimal(12,3), nu numeric(20,0), mo money, sm smallmoney, r real, \
           f float, b bit, c char(5), vc varchar(30), nc nchar(5), nv nvarchar(30), tx text, \
           nt ntext, bn binary(4), vb varbinary(10), im image, d date, t time(7), dt datetime, \
           dt3 datetime, sdt smalldatetime, dt2 datetime2(7), dto datetimeoffset(7), \
           u uniqueidentifier, x xml, v sql_variant, h hierarchyid, g geometry)",
        "insert into pythia_live_types values (1, 255, -32768, 2147483647, \
           -9223372036854775808, 123456789.125, 12345678901234567890, 12345.6789, \
           -214748.3648, 0.1, 0.1e0 + 0.2e0, 1, 'ab', 'it''s & C:\\x', N'\u{e9}', \
           N'gr\u{fc}\u{df}e \u{65e5}\u{672c}', 'text', N'ntext \u{fc}', 0x00ff, 0x0102, \
           0xdeadbeef, '2026-01-02', '12:34:56.1234567', '2026-01-02T03:04:05.997', \
           '2026-01-02T03:04:05.007', '2026-01-02T03:04:00', '2026-01-02 03:04:05.1234567', \
           '2026-01-02 03:04:05.1234567 +05:30', '6F9619FF-8B86-D011-B42D-00C04FC964FF', \
           N'<a b=\"1\">x &amp; y</a>', cast(5 as int), '/1/2/', \
           geometry::STGeomFromText('POINT(1 2)', 0))",
        "insert into pythia_live_types (id) values (2)",
        "create table pythia_live_floats (id int primary key, d float, r real)",
        // A CLR type as the key.
        "create table pythia_live_h (h hierarchyid primary key, v int)",
        "insert into pythia_live_h values ('/', 1), ('/1/', 2), ('/1/3/', 3)",
    ]);
    let floats = random_floats(200);
    for (n, (d, r)) in floats.iter().enumerate() {
        db.ok(&format!(
            "insert into pythia_live_floats values ({}, {d:e}, cast({r:e} as real))",
            n + 1
        ));
    }
    // Every type is checked but sql_variant, whose value would compare as
    // text; written back, it would hold an nvarchar, so it's left alone.
    assert_eq!(
        db.rewrite_all("pythia_live_types", "id", &["v"]),
        Vec::<String>::new()
    );
    db.checks_find_rows("pythia_live_types", "id");
    let (_, columns, rows) = db.editable("pythia_live_types", "id");
    let v = columns.iter().position(|c| c.name == "v").unwrap();
    assert_eq!(
        same(db.dialect, "v", &columns[v], rows[0][v].as_deref()),
        None
    );
    assert!(db.rewrite_all("pythia_live_floats", "id", &[]).is_empty());
    assert!(db.rewrite_all("pythia_live_h", "v", &[]).is_empty());
    db.script(&["drop table pythia_live_types, pythia_live_floats, pythia_live_h"]);
}

#[test]
fn sqlite_keys() {
    let (mut db, path) = Live::sqlite("keys");
    let attached = path.with_extension("other.db");
    let _ = std::fs::remove_file(&attached);
    db.script(&[
        "create table k (id text not null primary key, code text not null unique)",
        "create table \"Mixed Case\" (\"the id\" integer primary key, v)",
        "create table composite (a integer not null, b text not null, v, primary key (a, b))",
        "create table nullable_pk (p text primary key, u integer not null, v)",
        "create unique index nullable_pk_u on nullable_pk (u)",
        "create table loose (n integer unique, e text, v)",
        "create unique index loose_e on loose (upper(e))",
        "create unique index loose_partial on loose (v) where v > 0",
        "create table wr (a text, b integer, primary key (b, a)) without rowid",
        "create table shadow (rowid text, v)",
        "create view k_view as select * from k",
        "create temp table temp_first (t integer primary key)",
        "create table temp_first (m text primary key not null)",
        &format!("attach database '{}' as other", attached.display()),
        "create table other.k (other_id integer primary key)",
    ]);
    assert_eq!(db.key("k"), ["id"]);
    assert_eq!(db.key("K"), ["id"]);
    // The rowid itself, quoted names.
    assert_eq!(db.key("\"Mixed Case\""), ["the id"]);
    assert_eq!(db.key("composite"), ["a", "b"]);
    // A primary key that may hold NULLs (an old quirk of rowid tables)
    // gives way to a unique index on NOT NULL columns.
    assert_eq!(db.key("nullable_pk"), ["u"]);
    // Neither a nullable column, an expression nor a partial index: the
    // rowid.
    assert_eq!(db.key("loose"), ["rowid"]);
    assert_eq!(db.key("wr"), ["b", "a"]);
    // A column took the name `rowid`.
    assert_eq!(db.key("shadow"), ["_rowid_"]);
    // As SQLite looks names up: `temp` first, a schema when named.
    assert_eq!(db.key("temp_first"), ["t"]);
    assert_eq!(db.key("main.temp_first"), ["m"]);
    assert_eq!(db.key("other.k"), ["other_id"]);
    assert!(db.key("k_view").is_empty());
    assert!(db.key("missing").is_empty());
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&attached);
}

/// Every kind of value SQLite stores, in columns of each affinity and none,
/// and random floats, found by the checks and written back as shown; a
/// table keyed by its rowid, too.
#[test]
fn sqlite_values_find_their_rows() {
    let (mut db, path) = Live::sqlite("values");
    db.script(&[
        // A file thrown away after: no syncing for each statement.
        "pragma synchronous = off",
        "create table types (id integer primary key, i integer, r real, d decimal(10,2), \
           t text, vc varchar(20), b blob, dt date, ts timestamp, j json, n, bo boolean)",
        "insert into types values (1, -9223372036854775808, 0.1, 12.5, \
           'C:\\x ''q'' & é' || char(10) || 'new line', 'it''s', x'00ff10', \
           '2026-10-31', '2026-10-31 12:34:56.789', '{\"a\":  [1, 2.50]}', 'text', 1)",
        "insert into types values (2, 9223372036854775807, 1e300, 99, '', ' ', x'', \
           '2026-11-01', null, '[]', 7, 0)",
        "insert into types values (3, 0, -2.2250738585072014e-308, 0.333, 'x', 'y', \
           zeroblob(5000), null, null, null, 1.5, null)",
        "insert into types (id) values (4)",
        "create table floats (id integer primary key, d double, r real)",
        "create table plain (v text, n)",
        "insert into plain values ('one', 1), ('two', 2.5), (null, 'three')",
    ]);
    let floats = random_floats(200);
    for chunk in floats.chunks(50) {
        let values: Vec<String> = chunk
            .iter()
            .map(|(d, r)| format!("({d:e}, {:e})", f64::from(*r)))
            .collect();
        db.ok(&format!(
            "insert into floats (d, r) values {}",
            values.join(", ")
        ));
    }
    // A blob shown cut short isn't checked; everything else is.
    assert_eq!(db.rewrite_all("types", "id", &["b"]), Vec::<String>::new());
    assert!(db.rewrite_all("floats", "id", &[]).is_empty());
    assert!(db.rewrite_all("plain", "rowid", &[]).is_empty());
    db.checks_find_rows("types", "id");
    // The long blob: left out of the check, not compared as text.
    let (_, columns, rows) = db.editable("types", "id");
    let b = columns.iter().position(|c| c.name == "b").unwrap();
    assert!(same(db.dialect, "b", &columns[b], rows[2][b].as_deref()).is_none());
    assert!(same(db.dialect, "b", &columns[b], rows[0][b].as_deref()).is_some());
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A blob is one whatever the column's declared type (`image`,
/// `binary(16)`, none), also when its hex is all digits, and text in a
/// BLOB column is text: each is found by the check an edit makes and
/// written back as what it was.
#[test]
fn sqlite_blobs_by_their_values() {
    let (mut db, path) = Live::sqlite("blobs");
    db.script(&[
        "create table stored (id integer primary key, im image, bn binary(16), nt, bt blob, \
           digits blob)",
        "insert into stored values (1, x'cafe', x'00ff', x'cafe', 'hello world', x'313233')",
        "insert into stored values (2, x'10', x'', x'12', '00ff', x'10')",
    ]);
    let (_, columns, _) = db.editable("stored", "id");
    let types: Vec<&str> = columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(
        types,
        ["INTEGER", "BLOB", "BLOB", "BLOB", "TEXT", "BLOB", "INTEGER"]
    );
    assert!(db.rewrite_all("stored", "id", &[]).is_empty());
    let (_, rows) = db
        .query("select typeof(im), typeof(bn), typeof(nt), typeof(bt), typeof(digits) from stored");
    for row in rows {
        let kinds: Vec<&str> = row.iter().map(|v| v.as_deref().unwrap_or("")).collect();
        assert_eq!(kinds, ["blob", "blob", "blob", "text", "blob"]);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A query that renames columns, `select id, description as name, name as
/// title`: a cell under a label that names another column must not write
/// to it. Only columns of the table as they are are written, each to
/// itself; a renamed column or an expression changes nothing.
fn renamed_columns(db: &mut Live, create: &str) {
    db.script(&[
        "drop table if exists fd_renamed",
        create,
        "insert into fd_renamed values (1, 'same', 'same')",
    ]);
    let key = db.key("fd_renamed");
    let edit = Editable {
        table: "fd_renamed".into(),
        source: "select id, description as name, name as title, upper(name) as shout, \
                 r.name, r.* from fd_renamed r"
            .into(),
        key: RowKey::Columns(key),
    };
    let sql = edit_query(db.dialect, &edit).expect("an editable query");
    let (columns, rows) = db.query(&sql);
    let names: Vec<String> = columns.iter().map(|c| c.name.to_lowercase()).collect();
    assert_eq!(
        names[..8],
        [
            "id",
            "name",
            "title",
            "shout",
            "name",
            "id",
            "name",
            "description"
        ],
        "{:?}",
        db.dialect
    );
    // Every cell but the key's changed: the renamed ones and the
    // expression write nothing.
    let mut changes = Changes::default();
    for (c, value) in [(1, "x"), (2, "y"), (3, "z")] {
        changes
            .updates
            .entry(0)
            .or_default()
            .insert(c, Some(value.into()));
    }
    assert!(
        dml(db.dialect, &edit, &columns, &rows, &changes).is_empty(),
        "{:?}",
        dml(db.dialect, &edit, &columns, &rows, &changes)
    );
    for statement in dml(db.dialect, &edit, &columns, &rows, &changes) {
        db.ok(&statement.sql);
    }
    // The plain name, and the description from `*`.
    changes.updates.clear();
    changes
        .updates
        .entry(0)
        .or_default()
        .extend([(4, Some("new".to_owned())), (7, Some("told".to_owned()))]);
    for statement in dml(db.dialect, &edit, &columns, &rows, &changes) {
        assert!(
            matches!(db.ok(&statement.sql), Ran::Done(Some(1))),
            "{}",
            statement.sql
        );
    }
    let (_, values) = db.query("select name, description from fd_renamed");
    assert_eq!(
        values[0],
        [Some("new".to_owned()), Some("told".to_owned())],
        "{:?}",
        db.dialect
    );
    db.ok("drop table fd_renamed");
}

#[test]
fn renamed_columns_stay_read_only() {
    let create = "create table fd_renamed (id int primary key, name varchar(20), \
                  description varchar(20))";
    let (mut db, path) = Live::sqlite("renamed");
    renamed_columns(&mut db, create);
    drop(db);
    let _ = std::fs::remove_file(&path);
    for (env, kind) in [
        ("PYTHIA_TEST_MYSQL", DbKind::MySql),
        ("PYTHIA_TEST_POSTGRES", DbKind::Postgres),
        ("PYTHIA_TEST_MSSQL", DbKind::SqlServer),
    ] {
        if let Some(mut db) = Live::connect(env, kind) {
            renamed_columns(&mut db, create);
        }
    }
}

/// A binary value of 5000 bytes, which results hold cut short to 4000
/// with its size (#223; PostgreSQL's bytea comes whole): set to itself it is
/// left as it is, nothing written; edited, its row is found by the key
/// alone, the value cut short can't be compared with the server's.
fn cut_values(db: &mut Live, create: &str, value: &str, new: &str) {
    db.script(&[
        "drop table if exists fd_cut",
        create,
        &format!("insert into fd_cut values (1, {value}, 'x')"),
    ]);
    let (edit, columns, rows) = db.editable("fd_cut", "id");
    let b = columns.iter().position(|c| c.name == "b").unwrap();
    let n = columns.iter().position(|c| c.name == "n").unwrap();
    let cut = rows[0][b].as_deref().and_then(pythia_db::cut_size);
    let whole = db.dialect == Dialect::POSTGRES;
    assert_eq!(cut, (!whole).then_some(5000), "{:?}", db.dialect);
    let update = |cells: &[(usize, &str)]| Changes {
        updates: BTreeMap::from([(
            0,
            cells
                .iter()
                .map(|(c, v)| (*c, Some((*v).to_owned())))
                .collect(),
        )]),
        ..Changes::default()
    };
    let same = rows[0][b].clone().unwrap();
    let statements = dml(
        db.dialect,
        &edit,
        &columns,
        &rows,
        &update(&[(b, &same), (n, "y")]),
    );
    for statement in &statements {
        assert!(
            whole || !statement.sql.contains(" bytes)"),
            "{:?}: {}",
            db.dialect,
            statement.sql
        );
        let ran = db.run(&statement.sql);
        assert!(
            matches!(ran, Ok(Ran::Done(Some(1)))),
            "{ran:?}\n{}",
            statement.sql
        );
    }
    let (edit, columns, rows) = db.editable("fd_cut", "id");
    for statement in dml(
        db.dialect,
        &edit,
        &columns,
        &rows,
        &update(&[(b, new), (n, "z")]),
    ) {
        let ran = db.run(&statement.sql);
        assert!(
            matches!(ran, Ok(Ran::Done(Some(1)))),
            "{ran:?}\n{}",
            statement.sql
        );
    }
    let length = match db.dialect.kind() {
        DbKind::SqlServer => "datalength",
        _ => "length",
    };
    let check = db.count(&format!("select {length}(b) from fd_cut"));
    assert_eq!(check, "2", "{:?}", db.dialect);
    assert_eq!(db.count("select n from fd_cut"), "z");
    db.ok("drop table fd_cut");
}

#[test]
fn cut_values_are_neither_checked_nor_written() {
    let (mut db, path) = Live::sqlite("cut");
    cut_values(
        &mut db,
        "create table fd_cut (id integer primary key, b blob, n text)",
        "randomblob(5000)",
        "CAFE",
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
    let servers = [
        (
            "PYTHIA_TEST_MYSQL",
            DbKind::MySql,
            "create table fd_cut (id int primary key, b longblob, n varchar(5))",
            "repeat(unhex('AB'), 5000)",
            "CAFE",
        ),
        (
            "PYTHIA_TEST_POSTGRES",
            DbKind::Postgres,
            "create table fd_cut (id int primary key, b bytea, n varchar(5))",
            "decode(repeat('ab', 5000), 'hex')",
            r"\xcafe",
        ),
        (
            "PYTHIA_TEST_MSSQL",
            DbKind::SqlServer,
            "create table fd_cut (id int primary key, b varbinary(max), n nvarchar(5))",
            "cast(replicate(cast(0xAB as varchar(max)), 5000) as varbinary(max))",
            "0xCAFE",
        ),
    ];
    for (env, kind, create, value, new) in servers {
        if let Some(mut db) = Live::connect(env, kind) {
            cut_values(&mut db, create, value, new);
        }
    }
}

/// MySQL's BIT values, shown as hex (#253): each one typed again as shown
/// finds its row and writes itself, also on MariaDB, which compared the
/// `X'…'` check with the BIT as a decimal and refused the UPDATE ("Truncated
/// incorrect DECIMAL value"); and `1` typed into a BIT(1), `5` into a
/// BIT(8), as hex, which went as `X'1'`, a syntax error.
#[test]
fn mysql_bits() {
    let Some(mut db) = Live::connect("PYTHIA_TEST_MYSQL", DbKind::MySql) else {
        return;
    };
    db.script(&[
        "drop table if exists r41_bits",
        "create table r41_bits (id int primary key, b1 bit(1), b8 bit(8), b64 bit(64))",
        "insert into r41_bits values (1, b'0', b'10000001', 18446744073709551615), \
           (2, b'1', b'0', 0), (3, null, null, null)",
    ]);
    assert!(db.rewrite_all("r41_bits", "id", &[]).is_empty());
    let (edit, columns, rows) = db.editable("r41_bits", "id");
    assert_eq!(
        rows[0][1..4],
        [
            Some("00".into()),
            Some("81".into()),
            Some("FFFFFFFFFFFFFFFF".into())
        ]
    );
    let changes = Changes {
        updates: BTreeMap::from([(
            0,
            BTreeMap::from([(1, Some("1".into())), (2, Some("5".into()))]),
        )]),
        ..Changes::default()
    };
    for statement in dml(db.dialect, &edit, &columns, &rows, &changes) {
        let ran = db.run(&statement.sql);
        assert!(
            matches!(ran, Ok(Ran::Done(Some(1)))),
            "{ran:?}\n{}",
            statement.sql
        );
    }
    let (_, _, rows) = db.editable("r41_bits", "id");
    assert_eq!(rows[0][1..3], [Some("01".into()), Some("05".into())]);
    db.ok("drop table r41_bits");
}

/// A binary value typed as the grid shows it, `0x171066e1`, is those four
/// bytes (#256), also the one past 0x7f: MySQL, PostgreSQL and SQLite
/// stored the text's ten (`0x30783137…`).
fn binary_typed_as_shown(db: &mut Live, create: &str) {
    db.script(&[
        "drop table if exists r41_typed",
        create,
        "insert into r41_typed values (1, null)",
    ]);
    let (edit, columns, rows) = db.editable("r41_typed", "id");
    let changes = Changes {
        updates: BTreeMap::from([(0, BTreeMap::from([(1, Some("0x171066e1".into()))]))]),
        ..Changes::default()
    };
    for statement in dml(db.dialect, &edit, &columns, &rows, &changes) {
        let ran = db.run(&statement.sql);
        assert!(
            matches!(ran, Ok(Ran::Done(Some(1)))),
            "{ran:?}\n{}",
            statement.sql
        );
    }
    let (_, _, rows) = db.editable("r41_typed", "id");
    let shown = rows[0][1].as_deref().unwrap_or_default();
    let hex = shown.trim_start_matches("0x").trim_start_matches(r"\x");
    assert!(
        hex.eq_ignore_ascii_case("171066e1"),
        "{:?}: {shown}",
        db.dialect
    );
    let length = match db.dialect.kind() {
        DbKind::SqlServer => "datalength",
        _ => "length",
    };
    let bytes = db.count(&format!("select {length}(b) from r41_typed"));
    assert_eq!(bytes, "4", "{:?}", db.dialect);
    db.ok("drop table r41_typed");
}

#[test]
fn binary_values_typed_as_shown() {
    let (mut db, path) = Live::sqlite("typed");
    binary_typed_as_shown(
        &mut db,
        "create table r41_typed (id integer primary key, b blob)",
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
    let servers = [
        (
            "PYTHIA_TEST_MYSQL",
            DbKind::MySql,
            "create table r41_typed (id int primary key, b varbinary(16))",
        ),
        (
            "PYTHIA_TEST_POSTGRES",
            DbKind::Postgres,
            "create table r41_typed (id int primary key, b bytea)",
        ),
        (
            "PYTHIA_TEST_MSSQL",
            DbKind::SqlServer,
            "create table r41_typed (id int primary key, b varbinary(16))",
        ),
    ];
    for (env, kind, create) in servers {
        if let Some(mut db) = Live::connect(env, kind) {
            binary_typed_as_shown(&mut db, create);
        }
    }
}

/// On Oracle too: `0x171066e1` typed into a RAW is its four bytes, and so
/// into the RAW, BLOB and LONG RAW of a new row. Oracle's `hextoraw`
/// failed on the `0x` with ORA-01465.
#[test]
fn oracle_binary_values_typed_as_shown() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let tables = ["r54_typed", "r54_typed_long"];
    for table in tables {
        let _ = conn.execute(&format!("drop table {table} purge"), &[]);
    }
    for sql in [
        "create table r54_typed (id number primary key, r raw(16), b blob)",
        "insert into r54_typed (id) values (1)",
        "create table r54_typed_long (id number primary key, l long raw)",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    // The rows as the session shows them, with their ROWID.
    let fetch = |sql: &str| {
        let mut fetched = (Vec::new(), Vec::new());
        pythia_db::stream_query(&conn, sql, 100, |columns, rows| {
            fetched.0 = columns.to_vec();
            fetched.1.extend_from_slice(rows);
            Ok(())
        })
        .unwrap();
        fetched
    };
    let apply = |table: &str, changes: &Changes| {
        let edit = Editable {
            table: table.into(),
            source: format!("select * from {table} order by id"),
            key: RowKey::Rowid,
        };
        let (columns, rows) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
        for statement in dml(Dialect::ORACLE, &edit, &columns, &rows, changes) {
            let ran = conn
                .execute(&statement.sql, &[])
                .and_then(|s| s.row_count());
            assert!(matches!(ran, Ok(1)), "{ran:?}\n{}", statement.sql);
        }
    };
    let typed = || Some("0x171066e1".to_owned());
    let typed_row = |columns: usize| {
        let mut row = vec![Some("2".to_owned())];
        row.resize_with(columns, typed);
        row
    };
    apply(
        "r54_typed",
        &Changes {
            updates: BTreeMap::from([(0, BTreeMap::from([(1, typed())]))]),
            inserts: vec![typed_row(3)],
            ..Changes::default()
        },
    );
    let (_, rows) = fetch("select id, r, b, dbms_lob.getlength(b) from r54_typed order by id");
    let hex = Some("171066E1".to_owned());
    assert_eq!(
        rows,
        [
            vec![Some("1".to_owned()), hex.clone(), None, None],
            vec![Some("2".into()), hex.clone(), hex.clone(), Some("4".into())],
        ]
    );
    apply(
        "r54_typed_long",
        &Changes {
            inserts: vec![typed_row(2)],
            ..Changes::default()
        },
    );
    let (_, rows) = fetch("select l from r54_typed_long");
    assert_eq!(rows, [[hex]]);
    for table in tables {
        conn.execute(&format!("drop table {table} purge"), &[])
            .unwrap();
    }
}

/// A fetched row's CLOB, NCLOB, BLOB, LONG and LONG RAW can be changed, a
/// NULL too (#309): the UPDATE checked the value fetched with DECODE,
/// which fails with ORA-00932 on a LOB and ORA-00997 on a LONG. A LOB
/// typed so (the Instant Client's types) is checked with
/// `dbms_lob.compare`, which misses a value changed since; a LONG, and a
/// LOB the pure-Rust driver types LONG or LONG RAW, is left out.
#[test]
fn oracle_lobs_of_fetched_rows() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let tables = ["r65_lobs", "r65_long", "r65_long_raw"];
    for table in tables {
        let _ = conn.execute(&format!("drop table {table} purge"), &[]);
    }
    for sql in [
        "create table r65_lobs (id number primary key, c clob, n nclob, b blob)",
        "insert into r65_lobs values (1, 'abc', 'xyz', hextoraw('CAFE'))",
        "insert into r65_lobs (id) values (2)",
        "create table r65_long (id number primary key, l long)",
        "insert into r65_long values (1, 'abc')",
        "insert into r65_long (id) values (2)",
        "create table r65_long_raw (id number primary key, l long raw)",
        "insert into r65_long_raw values (1, hextoraw('CAFE'))",
        "insert into r65_long_raw (id) values (2)",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let fetch = |sql: &str| {
        let mut fetched = (Vec::new(), Vec::new());
        pythia_db::stream_query(&conn, sql, 100, |columns, rows| {
            fetched.0 = columns.to_vec();
            fetched.1.extend_from_slice(rows);
            Ok(())
        })
        .unwrap();
        fetched
    };
    let edit = |table: &str| Editable {
        table: table.into(),
        source: format!("select * from {table} order by id"),
        key: RowKey::Rowid,
    };
    // Each value of each row set to another, one UPDATE at a time.
    let new = |type_name: &str| match type_name {
        "BLOB" | "LONG RAW" => "0xBABE",
        _ => "new",
    };
    for table in tables {
        let edit = edit(table);
        let (columns, rows) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
        for r in 0..rows.len() {
            for (c, col) in columns.iter().enumerate().take(columns.len() - 1).skip(1) {
                let value = Some(new(&col.type_name).to_owned());
                let changes = Changes {
                    updates: BTreeMap::from([(r, BTreeMap::from([(c, value)]))]),
                    ..Changes::default()
                };
                for statement in dml(Dialect::ORACLE, &edit, &columns, &rows, &changes) {
                    let ran = conn
                        .execute(&statement.sql, &[])
                        .and_then(|s| s.row_count());
                    assert!(matches!(ran, Ok(1)), "{ran:?}\n{}", statement.sql);
                }
            }
        }
    }
    let (_, rows) = fetch(
        "select c, n, b, (select count(*) from r65_long where l is not null), \
           (select count(*) from r65_long_raw where l is not null) from r65_lobs order by id",
    );
    let changed = ["new", "new", "BABE", "2", "2"].map(|v| Some(v.to_owned()));
    assert_eq!(rows, [changed.clone(), changed]);

    // The check of a LOB typed as such finds the value fetched, a NULL
    // too, and not another.
    conn.execute(
        "update r65_lobs set c = null, n = null, b = null where id = 2",
        &[],
    )
    .unwrap();
    let (_, rows) = fetch("select rowid from r65_lobs order by id");
    for (name, type_name, value, other) in [
        ("C", "CLOB", "new", "abc"),
        ("N", "NCLOB", "new", "xyz"),
        ("B", "BLOB", "BABE", "CAFE"),
    ] {
        let col = Column {
            name: name.into(),
            type_name: type_name.into(),
            numeric: false,
        };
        let count = |row: usize, value: Option<&str>| {
            let rowid = rows[row][0].as_deref().unwrap();
            let check = same(Dialect::ORACLE, name, &col, value).unwrap();
            let sql = format!("select count(*) from r65_lobs where rowid = '{rowid}' and {check}");
            fetch(&sql).1[0][0].clone().unwrap()
        };
        assert_eq!(count(0, Some(value)), "1", "{type_name}");
        assert_eq!(count(0, Some(other)), "0", "{type_name}");
        assert_eq!(count(0, None), "0", "{type_name}");
        assert_eq!(count(1, None), "1", "{type_name}");
        assert_eq!(count(1, Some(value)), "0", "{type_name}");
    }
    for table in tables {
        conn.execute(&format!("drop table {table} purge"), &[])
            .unwrap();
    }
}

/// The check of a fetched BLOB over the 2000 bytes a `hextoraw` takes,
/// typed BLOB as the Instant Client types it (#314): it finds the value,
/// and not one with another byte or fewer; with the value in one literal
/// it failed with ORA-01704.
#[test]
fn oracle_long_blobs_are_checked() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let _ = conn.execute("drop table r65_long_blob purge", &[]);
    for sql in [
        "create table r65_long_blob (id number primary key, b blob)",
        "declare
           v blob;
         begin
           dbms_lob.createtemporary(v, true);
           dbms_lob.writeappend(v, 2000, utl_raw.copies(hextoraw('0102'), 1000));
           dbms_lob.writeappend(v, 1000, utl_raw.copies(hextoraw('0304'), 500));
           insert into r65_long_blob values (1, v);
         end;",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let count = |sql: &str| {
        let mut count = None;
        pythia_db::stream_query(&conn, sql, 10, |_, rows| {
            if let Some(row) = rows.first() {
                count = row[0].clone();
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}\n{sql}"));
        count.unwrap()
    };
    let hex = count("select b from r65_long_blob");
    assert_eq!(hex.len(), 6000);
    let col = Column {
        name: "B".into(),
        type_name: "BLOB".into(),
        numeric: false,
    };
    let found = |value: &str| {
        let check = same(Dialect::ORACLE, "B", &col, Some(value)).unwrap();
        count(&format!("select count(*) from r65_long_blob where {check}"))
    };
    assert_eq!(found(&hex), "1");
    let other = format!("{}05", &hex[..5998]);
    assert_eq!(found(&other), "0");
    assert_eq!(found(&hex[..5000]), "0");
    conn.execute("drop table r65_long_blob purge", &[]).unwrap();
}

/// A fetched CLOB over the 4000 bytes a literal holds is changed, and a
/// new row takes one (#317): the UPDATE and the INSERT failed with
/// ORA-01704.
#[test]
fn oracle_long_clobs_can_be_changed() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let _ = conn.execute("drop table r65_long_clob purge", &[]);
    for sql in [
        "create table r65_long_clob (id number primary key, c clob)",
        "insert into r65_long_clob values (1, to_clob(rpad('x', 4000, 'x')) || rpad('y', 1000, 'y'))",
        "commit",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let fetch = |sql: &str| {
        let mut fetched = (Vec::new(), Vec::new());
        pythia_db::stream_query(&conn, sql, 100, |columns, rows| {
            fetched.0 = columns.to_vec();
            fetched.1.extend_from_slice(rows);
            Ok(())
        })
        .unwrap();
        fetched
    };
    let edit = Editable {
        table: "r65_long_clob".into(),
        source: "select * from r65_long_clob order by id".into(),
        key: RowKey::Rowid,
    };
    let (columns, rows) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
    let changed = format!("{}z{}", "x".repeat(4000), "y".repeat(999));
    let new = format!("it's & {}", "\u{e9}".repeat(3000));
    let changes = Changes {
        updates: BTreeMap::from([(0, BTreeMap::from([(1, Some(changed.clone()))]))]),
        inserts: vec![vec![Some("2".into()), Some(new.clone()), None]],
        ..Changes::default()
    };
    // As the window runs them: a CLOB the pure-Rust driver types LONG is
    // bound past 4000 bytes (#293).
    let mut db = Live::connect("PYTHIA_TEST_DB", DbKind::Oracle).expect("PYTHIA_TEST_DB");
    for statement in dml(Dialect::ORACLE, &edit, &columns, &rows, &changes) {
        let ran = db.apply(&statement);
        let sql = &statement.sql[..statement.sql.len().min(200)];
        assert!(matches!(ran, Ok(Ran::Done(Some(1)))), "{ran:?}\n{sql}");
    }
    let (_, rows) = fetch("select c from r65_long_clob order by id");
    assert_eq!(rows, [[Some(changed)], [Some(new)]]);
    conn.execute("drop table r65_long_clob purge", &[]).unwrap();
}

/// Values typed into Oracle cells that no literal holds are bound (#293):
/// 3000 bytes in a BLOB and in a LONG RAW, which `hextoraw` refuses past
/// 2000 (ORA-01704), 5000 bytes of text in a LONG (ORA-64203 for its
/// literal of parts), and 260000 characters in a CLOB, past the parts
/// its literal takes (ORA-01489); in a fetched row and in a new one, read
/// back as typed.
#[test]
fn oracle_values_past_their_literals() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let tables = ["lob_typed", "lob_typed_raw", "lob_typed_long"];
    for table in tables {
        let _ = conn.execute(&format!("drop table {table} purge"), &[]);
    }
    for sql in [
        "create table lob_typed (id number primary key, b blob, c clob)",
        "insert into lob_typed values (1, hextoraw('CAFE'), 'old')",
        "create table lob_typed_raw (id number primary key, l long raw)",
        "insert into lob_typed_raw values (1, hextoraw('CAFE'))",
        "create table lob_typed_long (id number primary key, l long)",
        "insert into lob_typed_long values (1, 'old')",
        // The window's session applies the changes.
        "commit",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let fetch = |sql: &str| {
        let mut fetched = (Vec::new(), Vec::new());
        pythia_db::stream_query(&conn, sql, 100, |columns, rows| {
            fetched.0 = columns.to_vec();
            fetched.1.extend_from_slice(rows);
            Ok(())
        })
        .unwrap();
        fetched
    };
    let mut db = Live::connect("PYTHIA_TEST_DB", DbKind::Oracle).expect("PYTHIA_TEST_DB");
    let hex = |seed: usize| -> String {
        (0..3000)
            .map(|i| format!("{:02X}", (i * 7 + seed) % 256))
            .collect()
    };
    let long_text = |n: usize, c: &str| c.repeat(n);
    let cases = [
        (
            "lob_typed",
            vec![(1, format!("0x{}", hex(1))), (2, long_text(260_000, "é"))],
            vec![hex(1), long_text(260_000, "é")],
        ),
        ("lob_typed_raw", vec![(1, hex(2))], vec![hex(2)]),
        (
            "lob_typed_long",
            vec![(1, long_text(2500, "é"))],
            vec![long_text(2500, "é")],
        ),
    ];
    for (table, typed, read) in cases {
        let edit = Editable {
            table: table.into(),
            source: format!("select * from {table} order by id"),
            key: RowKey::Rowid,
        };
        let (columns, rows) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
        let mut new = vec![Some("2".to_owned())];
        new.resize(columns.len(), None);
        for (c, v) in &typed {
            new[*c] = Some(v.clone());
        }
        let changes = Changes {
            updates: BTreeMap::from([(
                0,
                typed.iter().map(|(c, v)| (*c, Some(v.clone()))).collect(),
            )]),
            inserts: vec![new],
            ..Changes::default()
        };
        for statement in dml(Dialect::ORACLE, &edit, &columns, &rows, &changes) {
            assert!(
                !statement.binds.is_empty(),
                "{table}: {}",
                &statement.sql[..100.min(statement.sql.len())]
            );
            let ran = db.apply(&statement);
            assert!(matches!(ran, Ok(Ran::Done(Some(1)))), "{table}: {ran:?}");
        }
        let names: Vec<&str> = typed
            .iter()
            .map(|(c, _)| columns[*c].name.as_str())
            .collect();
        let (_, rows) = fetch(&format!(
            "select {} from {table} order by id",
            names.join(", ")
        ));
        let read: Vec<Option<String>> = read.into_iter().map(Some).collect();
        assert!(
            rows.len() == 2 && rows.iter().all(|r| *r == read),
            "{table}: not as typed"
        );
    }
    for table in tables {
        conn.execute(&format!("drop table {table} purge"), &[])
            .unwrap();
    }
}

/// BINARY_DOUBLE and BINARY_FLOAT values beyond NUMBER's range, the
/// infinities and NaN find their rows and are written back as they were,
/// into new rows too (#339): as NUMBERs, 1e300 failed with ORA-01426,
/// -2e-300 was 0, and Inf and NaN were compared as the text 'inf'. A
/// file's text loaded into a cell is bound (#293), and read so too.
#[test]
fn oracle_binary_floats_find_their_rows() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let _ = conn.execute("drop table r76_floats purge", &[]);
    for sql in [
        "create table r76_floats (id number primary key, d binary_double, f binary_float)",
        "insert into r76_floats values (1, 1e300d, 3.4028235e38f)",
        "insert into r76_floats values (2, -2e-300d, 1e-40f)",
        "insert into r76_floats values (3, binary_double_infinity, binary_float_infinity)",
        "insert into r76_floats values (4, -binary_double_infinity, -binary_float_infinity)",
        "insert into r76_floats values (5, binary_double_nan, binary_float_nan)",
        "insert into r76_floats values (6, 0.1d, 0.1f)",
        // The window's session applies the changes.
        "commit",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let fetch = |sql: &str| {
        let mut fetched = (Vec::new(), Vec::new());
        pythia_db::stream_query(&conn, sql, 100, |columns, rows| {
            fetched.0 = columns.to_vec();
            fetched.1.extend_from_slice(rows);
            Ok(())
        })
        .unwrap();
        fetched
    };
    let edit = Editable {
        table: "r76_floats".into(),
        source: "select * from r76_floats order by id".into(),
        key: RowKey::Rowid,
    };
    let (columns, rows) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
    // Each value set to itself, and each row again as a new one.
    let mut changes = Changes::default();
    for (r, row) in rows.iter().enumerate() {
        changes.updates.insert(
            r,
            BTreeMap::from([(1, row[1].clone()), (2, row[2].clone())]),
        );
        changes.inserts.push(vec![
            Some((r + 11).to_string()),
            row[1].clone(),
            row[2].clone(),
            None,
        ]);
    }
    changes
        .loaded
        .insert((RowRef::Existing(5), 1), Loaded::Text("-1e-300".into()));
    // As the window runs them.
    let mut db = Live::connect("PYTHIA_TEST_DB", DbKind::Oracle).expect("PYTHIA_TEST_DB");
    for statement in dml(Dialect::ORACLE, &edit, &columns, &rows, &changes) {
        let ran = db.apply(&statement);
        assert!(
            matches!(ran, Ok(Ran::Done(Some(1)))),
            "{ran:?}\n{}",
            statement.sql
        );
    }
    let (_, again) = fetch("select d, f from r76_floats order by id");
    let mut values: Vec<Vec<Cell>> = rows.iter().map(|row| row[1..3].to_vec()).collect();
    let copies = values.clone();
    values[5][0] = Some("-1e-300".into());
    assert_eq!(again, [values, copies].concat());
    conn.execute("drop table r76_floats purge", &[]).unwrap();
}

/// A fetched row's JSON and VECTOR can be changed, to NULL too, and new
/// rows take them (#340): the UPDATE checked the value fetched with DECODE,
/// which fails with ORA-40796 on JSON and ORA-00932 on a VECTOR. JSON is
/// checked with `json_equal`, which finds the value fetched (its keys in
/// another order) and not another; a VECTOR, which Oracle can't compare
/// (ORA-22848), is left out of the check.
#[test]
fn oracle_json_and_vectors_of_fetched_rows() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let _ = conn.execute("drop table r76_json purge", &[]);
    for sql in [
        "create table r76_json (id number primary key, j json, v vector(3, float32))",
        r#"insert into r76_json values (1, '{"b":1,"a":[1,2.5,"x"],"c":"H&L"}', '[1,2,3]')"#,
        r#"insert into r76_json values (2, '"text"', '[0.5,-2,4]')"#,
        "insert into r76_json values (3, 'null', null)",
        "insert into r76_json (id) values (4)",
        // The window's session applies the changes.
        "commit",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let fetch = |sql: &str| {
        let mut fetched = (Vec::new(), Vec::new());
        pythia_db::stream_query(&conn, sql, 100, |columns, rows| {
            fetched.0 = columns.to_vec();
            fetched.1.extend_from_slice(rows);
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}\n{sql}"));
        fetched
    };
    let edit = Editable {
        table: "r76_json".into(),
        source: "select * from r76_json order by id".into(),
        key: RowKey::Rowid,
    };
    let (columns, rows) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
    assert_eq!(columns[1].type_name, "JSON");
    assert_eq!(columns[2].type_name, "VECTOR");
    assert_eq!(
        rows[0][1].as_deref(),
        Some(r#"{"a":[1,2.5,"x"],"b":1,"c":"H&L"}"#)
    );

    // The check of each JSON finds its row's value, and not another's.
    for (r, row) in rows.iter().enumerate() {
        let rowid = row[3].as_deref().unwrap();
        for (other, value) in rows.iter().map(|row| row[1].as_deref()).enumerate() {
            let check = same(Dialect::ORACLE, "J", &columns[1], value).unwrap();
            let sql = format!("select count(*) from r76_json where rowid = '{rowid}' and {check}");
            let count = fetch(&sql).1[0][0].clone().unwrap();
            assert_eq!(count, if other == r { "1" } else { "0" }, "{sql}");
        }
        assert_eq!(
            same(Dialect::ORACLE, "V", &columns[2], row[2].as_deref()),
            None
        );
    }

    // As the window runs them.
    let mut db = Live::connect("PYTHIA_TEST_DB", DbKind::Oracle).expect("PYTHIA_TEST_DB");
    let mut apply = |changes: &Changes| {
        for statement in dml(Dialect::ORACLE, &edit, &columns, &rows, changes) {
            let ran = db.apply(&statement);
            assert!(
                matches!(ran, Ok(Ran::Done(Some(1)))),
                "{ran:?}\n{}",
                statement.sql
            );
        }
    };
    // Each value set to itself, one UPDATE at a time.
    for (r, row) in rows.iter().enumerate() {
        for c in [1, 2] {
            apply(&Changes {
                updates: BTreeMap::from([(r, BTreeMap::from([(c, row[c].clone())]))]),
                ..Changes::default()
            });
        }
    }
    let (_, again) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
    assert_eq!(again, rows);

    // Then to others, the second row's to NULL, the third's from a file,
    // which is bound (#293), and two new rows.
    let new = |r: usize| (r != 1).then(|| (format!(r#"{{"row":{r}}}"#), format!("[{r},{r},{r}]")));
    let mut changes = Changes::default();
    for r in 0..rows.len() {
        let (j, v) = new(r).unzip();
        changes.updates.insert(r, BTreeMap::from([(1, j), (2, v)]));
    }
    changes.inserts = vec![
        vec![
            Some("11".into()),
            Some(r#"{"new":[true,null]}"#.into()),
            Some("[1.5,2.5,3.5]".into()),
            None,
        ],
        vec![Some("12".into()), Some(r#""x""#.into()), None, None],
    ];
    let file = Loaded::Text(r#"{"file":[1,2]}"#.into());
    changes.loaded.insert((RowRef::Existing(2), 1), file);
    apply(&changes);
    let (_, again) = fetch("select id, j, v from r76_json order by id");
    let cells = |values: [Option<&str>; 3]| values.map(|v| v.map(str::to_owned)).to_vec();
    assert_eq!(
        again,
        [
            cells([Some("1"), Some(r#"{"row":0}"#), Some("[0,0,0]")]),
            cells([Some("2"), None, None]),
            cells([Some("3"), Some(r#"{"file":[1,2]}"#), Some("[2,2,2]")]),
            cells([Some("4"), Some(r#"{"row":3}"#), Some("[3,3,3]")]),
            cells([
                Some("11"),
                Some(r#"{"new":[true,null]}"#),
                Some("[1.5,2.5,3.5]")
            ]),
            cells([Some("12"), Some(r#""x""#), None]),
        ]
    );
    conn.execute("drop table r76_json purge", &[]).unwrap();
}

/// A fetched JSON and VECTOR over the 4000 bytes a literal holds is
/// changed, and a new row takes them (#342): the UPDATE and the INSERT
/// failed with ORA-01704. They stay literals of parts, also past the
/// length from which a CLOB's text is bound (#293): bound as a LONG,
/// a VECTOR fails with ORA-01461, a JSON next to another LONG with
/// ORA-40441.
#[test]
fn oracle_long_json_and_vectors_can_be_changed() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let _ = conn.execute("drop table r76_long_json purge", &[]);
    for sql in [
        "create table r76_long_json (id number primary key, j json, v vector(*, float32))",
        "insert into r76_long_json \
           select 1, json_arrayagg(json_object('k' value level, 's' value 'it''s & \u{e9}') \
                    returning json), \
                  to_vector(json_arrayagg(level / 7 returning clob), *, float32) \
             from dual connect by level <= 1000",
        // The window's session applies the changes.
        "commit",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let fetch = |sql: &str| {
        let mut fetched = (Vec::new(), Vec::new());
        pythia_db::stream_query(&conn, sql, 100, |columns, rows| {
            fetched.0 = columns.to_vec();
            fetched.1.extend_from_slice(rows);
            Ok(())
        })
        .unwrap();
        fetched
    };
    let edit = Editable {
        table: "r76_long_json".into(),
        source: "select * from r76_long_json order by id".into(),
        key: RowKey::Rowid,
    };
    let mut db = Live::connect("PYTHIA_TEST_DB", DbKind::Oracle).expect("PYTHIA_TEST_DB");
    let mut apply = |changes: &Changes| {
        let (columns, rows) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
        for statement in dml(Dialect::ORACLE, &edit, &columns, &rows, changes) {
            assert!(statement.binds.is_empty());
            let ran = db.apply(&statement);
            let sql = &statement.sql[..statement.sql.len().min(200)];
            assert!(matches!(ran, Ok(Ran::Done(Some(1)))), "{ran:?}\n{sql}");
        }
        rows
    };
    let (_, rows) = fetch("select j, v from r76_long_json");
    let (json, vector) = (rows[0][0].clone().unwrap(), rows[0][1].clone().unwrap());
    assert!(json.len() > 20_000 && vector.len() > 4000);
    let changed = json.replacen(r#""k":1,"#, r#""k":0,"#, 1);
    let other = format!("[{}]", vec!["0.5"; 2000].join(","));
    // 300,000 characters each.
    let huge_json = format!(r#"["{}"]"#, "x".repeat(300_000));
    let huge_vector = format!("[{}]", vec!["0.25"; 60_000].join(","));
    let row =
        |id: &str, j: &str, v: &str| vec![Some(id.into()), Some(j.into()), Some(v.into()), None];
    apply(&Changes {
        updates: BTreeMap::from([(
            0,
            BTreeMap::from([(1, Some(changed.clone())), (2, Some(other.clone()))]),
        )]),
        inserts: vec![row("2", &json, &vector), row("3", &huge_json, &huge_vector)],
        ..Changes::default()
    });
    // The huge JSON fetched is changed too, and checked.
    let fetched = apply(&Changes {
        updates: BTreeMap::from([(2, BTreeMap::from([(1, Some(json.clone()))]))]),
        ..Changes::default()
    });
    assert_eq!(fetched[2][1].as_ref(), Some(&huge_json));
    let (_, again) = fetch("select j, v from r76_long_json order by id");
    let cells = |j: &str, v: &str| vec![Some(j.to_owned()), Some(v.to_owned())];
    assert!(
        again
            == [
                cells(&changed, &other),
                cells(&json, &vector),
                cells(&json, &huge_vector)
            ],
        "not as written"
    );
    conn.execute("drop table r76_long_json purge", &[]).unwrap();
}

/// Oracle JSON's scalars of types JSON text has none of show as Oracle's
/// extended objects and keep their types (#370): dates, timestamps,
/// intervals, binary values and binary numbers showed as JSON strings and
/// numbers, so that the check of each value fetched found no row (a
/// conflict), and what was written had strings and numbers. A fetched
/// value set to itself or with a member changed keeps every other scalar
/// as it was, also where the driver reads a DATE, a TIMESTAMP and a
/// TIMESTAMP WITH TIME ZONE in UTC alike; a new row takes the extended
/// objects typed; objects MongoDB writes so (`{"$oid":…}`) stay objects.
#[test]
fn oracle_json_extended_scalars() {
    let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
        return;
    };
    let _ = conn.execute("drop table r84_json purge", &[]);
    for sql in [
        "create table r84_json (id number primary key, j json)",
        "insert into r84_json values (1, json{'a': date '2020-01-01', 'b': 1.5d, 'c': 1.5f, \
         'd': hextoraw('ABCD'), 'e': interval '1 02:03:04.5' day to second, \
         'f': interval '-1-2' year to month, 'g': 'x', 'h': 1, 'n': binary_double_nan, \
         's': timestamp '2020-01-01 10:00:00', 't': timestamp '2020-01-01 10:00:00.123456789', \
         'u': to_timestamp_tz('2020-01-01 10:00:00.5 +00:00', 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'), \
         'z': to_timestamp_tz('2020-01-01 10:00:00 +02:00', 'YYYY-MM-DD HH24:MI:SS TZH:TZM'), \
         'l': json_array(date '1999-12-31', -binary_float_infinity, 'y' returning json)})",
        "insert into r84_json values (2, json_scalar(1e300d))",
        r#"insert into r84_json values (3, json_transform(json('{"_id":{"$oid":"5f1b2c3d4e5f6a7b8c9d0e1f"},"&":{"$numberDouble":"1.5"}}'), set '$.d' = date '2020-01-01'))"#,
        "insert into r84_json values (4, json('{\"plain\":[1,\"two\"]}'))",
        "commit",
    ] {
        conn.execute(sql, &[])
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }
    let fetch = |sql: &str| {
        let mut fetched = (Vec::new(), Vec::new());
        pythia_db::stream_query(&conn, sql, 100, |columns, rows| {
            fetched.0 = columns.to_vec();
            fetched.1.extend_from_slice(rows);
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}\n{sql}"));
        fetched
    };
    // As Oracle has them, every type to be seen.
    let stored = || {
        fetch("select json_serialize(j extended) from r84_json order by id")
            .1
            .into_iter()
            .map(|row| row[0].clone().unwrap_or_default())
            .collect::<Vec<_>>()
    };
    let before = stored();
    let edit = Editable {
        table: "r84_json".into(),
        source: "select * from r84_json order by id".into(),
        key: RowKey::Rowid,
    };
    let (columns, rows) = fetch(&edit_query(Dialect::ORACLE, &edit).unwrap());
    let shown: Vec<&str> = rows.iter().map(|row| row[1].as_deref().unwrap()).collect();
    assert_eq!(
        shown,
        [
            concat!(
                r#"{"a":{"$oracleDate":"2020-01-01T00:00:00"},"b":{"$numberDouble":1.5},"#,
                r#""c":{"$numberFloat":1.5},"d":{"$rawhex":"ABCD"},"#,
                r#""e":{"$intervalDaySecond":"P1DT2H3M4.5S"},"f":{"$intervalYearMonth":"-P1Y2M"},"#,
                r#""g":"x","h":1,"#,
                r#""l":[{"$oracleDate":"1999-12-31T00:00:00"},{"$numberFloat":"-Inf"},"y"],"#,
                r#""n":{"$numberDouble":"Nan"},"s":{"$oracleDate":"2020-01-01T10:00:00"},"#,
                r#""t":{"$oracleTimestamp":"2020-01-01T10:00:00.123456789"},"#,
                r#""u":{"$oracleTimestamp":"2020-01-01T10:00:00.5"},"#,
                r#""z":{"$oracleTimestampTZ":"2020-01-01T10:00:00+02:00"}}"#
            ),
            r#"{"$numberDouble":1e300}"#,
            r#"{"&":{"$numberDouble":"1.5"},"_id":{"$oid":"5f1b2c3d4e5f6a7b8c9d0e1f"},"d":{"$oracleDate":"2020-01-01T00:00:00"}}"#,
            r#"{"plain":[1,"two"]}"#,
        ]
    );

    // The check of each value finds its row, and not another's.
    for (r, row) in rows.iter().enumerate() {
        let rowid = row[2].as_deref().unwrap();
        for (other, value) in rows.iter().map(|row| row[1].as_deref()).enumerate() {
            let check = same(Dialect::ORACLE, "J", &columns[1], value).unwrap();
            let sql = format!("select count(*) from r84_json where rowid = '{rowid}' and {check}");
            let count = fetch(&sql).1[0][0].clone().unwrap();
            assert_eq!(count, if other == r { "1" } else { "0" }, "{sql}");
        }
    }

    // Each set to itself, as the window runs it: every scalar as it was.
    let mut db = Live::connect("PYTHIA_TEST_DB", DbKind::Oracle).expect("PYTHIA_TEST_DB");
    let mut apply = |changes: &Changes| {
        for statement in dml(Dialect::ORACLE, &edit, &columns, &rows, changes) {
            let ran = db.apply(&statement);
            assert!(
                matches!(ran, Ok(Ran::Done(Some(1)))),
                "{ran:?}\n{}",
                statement.sql
            );
        }
    };
    for (r, row) in rows.iter().enumerate() {
        apply(&Changes {
            updates: BTreeMap::from([(r, BTreeMap::from([(1, row[1].clone())]))]),
            ..Changes::default()
        });
    }
    assert_eq!(stored(), before);

    // A member changed, one added: the others as they were.
    let changed = shown[0].replace(r#""g":"x""#, r#""g":"changed""#).replace(
        r#""a":{"$oracleDate":"2020-01-01T00:00:00"}"#,
        r#""a":{"$oracleDate":"2020-01-02T00:00:00"},"added":{"$intervalYearMonth":"P3Y"}"#,
    );
    apply(&Changes {
        updates: BTreeMap::from([(0, BTreeMap::from([(1, Some(changed))]))]),
        ..Changes::default()
    });
    let expected = before[0].replace(r#""g":"x""#, r#""g":"changed""#).replace(
        r#""a":{"$oracleDate":"2020-01-01T00:00:00"}"#,
        r#""a":{"$oracleDate":"2020-01-02T00:00:00"}"#,
    );
    let after = stored();
    assert_eq!(
        after[0],
        format!(
            "{},{}}}",
            &expected[..expected.len() - 1],
            r#""added":{"$intervalYearMonth":"P3Y"}"#
        )
    );

    // New rows take the extended objects typed, MongoDB's stay objects.
    apply(&Changes {
        inserts: vec![
            vec![Some("11".into()), Some(shown[0].into()), None],
            vec![Some("12".into()), Some(shown[2].into()), None],
            vec![
                Some("13".into()),
                Some(r#"{"$oracleTimestampTZ":"2021-02-03T04:05:06.7-05:30"}"#.into()),
                None,
            ],
        ],
        ..Changes::default()
    });
    let after = stored();
    // The driver can't tell s, a TIMESTAMP, and u, in UTC, from a DATE
    // and a TIMESTAMP.
    assert_eq!(
        after[4],
        before[0]
            .replace(
                r#""s":{"$oracleTimestamp":"2020-01-01T10:00:00"}"#,
                r#""s":{"$oracleDate":"2020-01-01T10:00:00"}"#
            )
            .replace(
                r#""u":{"$oracleTimestampTZ":"2020-01-01T10:00:00.500000Z"}"#,
                r#""u":{"$oracleTimestamp":"2020-01-01T10:00:00.500000"}"#
            )
    );
    assert_eq!(after[5], before[2]);
    assert_eq!(
        after[6],
        r#"{"$oracleTimestampTZ":"2021-02-03T04:05:06.700000-05:30"}"#
    );
    conn.execute("drop table r84_json purge", &[]).unwrap();
}
