//! Editing results against the databases: the key comes from the table the
//! query read, and every value of every type is found again by the check an
//! edit makes and written back the way it was shown. Skipped unless
//! `PYTHIA_TEST_POSTGRES` or `PYTHIA_TEST_MSSQL` is set
//! (`user/password@host:port/db`).

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

use pythia_db::{
    Cell, Client, Column, ConnectParams, DbError, DbEvent, DbKind, DbRequest, Dialect, Session,
};
use pythia_ui::Changes;

use super::{Editable, RowKey, dml, edit_query, row_key, same};

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
        Some(live)
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
        self.session.send(DbRequest::Execute {
            id: 1,
            sql: sql.into(),
            max_rows: 100_000,
            binds: Vec::new(),
        });
        let mut columns = Vec::new();
        let mut outcome = None;
        // The session's transaction is reported last.
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
                DbEvent::Transaction { .. } => {
                    if let Some(outcome) = outcome {
                        return outcome;
                    }
                }
                _ => {}
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
    ]);
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
