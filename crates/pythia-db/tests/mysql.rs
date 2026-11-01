//! MySQL sessions against a real server.
//!
//! Skipped unless `PYTHIA_TEST_MYSQL` is set to `user/password@host[:port]/database`,
//! e.g. the MySQL container from `docker-compose.yml`:
//! `PYTHIA_TEST_MYSQL=pythia/pythia@localhost:3306/pythia cargo test -p pythia-db --test mysql`
//!
//! The SSH tunnel test also needs `PYTHIA_TEST_SSH=user:password@host[:port]`
//! and `PYTHIA_TEST_MYSQL_VIA_SSH`, the database as the SSH server sees it
//! (the `ssh` service: `pythia:pythia@localhost:2222` and
//! `pythia/pythia@mysql:3306/pythia`).

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use pythia_db::ssh::{SshParams, Tunnel};
use pythia_db::{
    ConnectParams, DbEvent, DbKind, DbRequest, Detail, Dialect, MetaQuery, MetaResult, ObjectType,
    Session,
};

fn params() -> Option<ConnectParams> {
    mysql_params(&std::env::var("PYTHIA_TEST_MYSQL").ok()?)
}

/// From `user/password@connect_string`.
fn mysql_params(spec: &str) -> Option<ConnectParams> {
    let (credentials, connect_string) = spec.rsplit_once('@')?;
    let (username, password) = credentials.split_once('/')?;
    Some(ConnectParams {
        kind: DbKind::MySql,
        username: username.into(),
        password: password.into(),
        connect_string: connect_string.into(),
        privilege: Default::default(),
        init_sql: Vec::new(),
        ssh: None,
    })
}

/// The database through the SSH server, trusting the host keys kept in
/// `known_hosts`.
fn ssh_params(known_hosts: &Path) -> Option<ConnectParams> {
    let ssh = std::env::var("PYTHIA_TEST_SSH").ok()?;
    let mut params = mysql_params(&std::env::var("PYTHIA_TEST_MYSQL_VIA_SSH").ok()?)?;
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
                assert_eq!(server.dialect, Dialect::MYSQL);
                assert!(server.release >= (5, 7), "{server:?}");
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

    fn run(&mut self, id: u64, sql: &str) -> DbEvent {
        self.session.send(DbRequest::Execute {
            id,
            sql: sql.into(),
            max_rows: 100,
            binds: vec![],
        });
        self.next_result()
    }

    /// Runs a statement that must succeed; returns whether a transaction is
    /// pending after it (reported last).
    fn ok(&mut self, sql: &str) -> bool {
        self.session.send(DbRequest::Execute {
            id: 0,
            sql: sql.into(),
            max_rows: 100,
            binds: vec![],
        });
        loop {
            match self.next() {
                DbEvent::Transaction { pending } => return pending,
                DbEvent::Failed { error, .. } => panic!("{sql}: {error}"),
                _ => {}
            }
        }
    }

    /// The first value of a query's first row.
    fn scalar(&mut self, sql: &str) -> Option<String> {
        let DbEvent::QueryStarted { .. } = self.run(0, sql) else {
            panic!("{sql}: expected a query")
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
        "with recursive r (n) as (select 1 union all select n + 1 from r where n < {n}) \
         select n, concat('row ', n) as label from r"
    )
}

#[test]
fn query_and_fetch_more() {
    let Some(mut h) = Harness::connect() else {
        eprintln!("PYTHIA_TEST_MYSQL not set, skipping");
        return;
    };
    h.session.send(DbRequest::Execute {
        id: 1,
        sql: numbers(25),
        max_rows: 10,
        binds: vec![],
    });
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
        "select cast(1.50 as decimal(5,2)) d, date '2024-02-29' dt, \
                timestamp '2024-02-29 13:45:01' ts, null n, x'00ff' b, 'grüße' s",
    ) else {
        panic!("expected QueryStarted")
    };
    let types: Vec<&str> = columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(types[..3], ["DECIMAL", "DATE", "DATETIME"]);
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
            Some("00FF".into()),
            Some("grüße".into()),
        ]
    );

    h.session.send(DbRequest::Execute {
        id: 2,
        sql: "select :a + 1 as x, :B is null as y, :a as again".into(),
        max_rows: 10,
        binds: vec![("A".into(), Some("41".into())), ("b".into(), None)],
    });
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(rows[0][0].as_deref(), Some("42"));
    assert_eq!(rows[0][1].as_deref(), Some("1"));
    assert_eq!(rows[0][2].as_deref(), Some("41"));
}

#[test]
fn errors_carry_mysql_code_and_position() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let DbEvent::Failed { error, .. } = h.run(1, "select * from no_such_table") else {
        panic!("expected Failed")
    };
    assert_eq!(error.code, Some(1146));
    assert!(
        error.message.starts_with("ERROR 1146 (42S02): "),
        "{}",
        error.message
    );
    assert!(!error.is_connection_lost());

    let sql = "select *\n  frm dual";
    let DbEvent::Failed { error, .. } = h.run(2, sql) else {
        panic!("expected Failed")
    };
    assert_eq!(error.code, Some(1064));
    assert_eq!(error.offset, Some(11), "{}", error.message);
}

/// A script is split where the server ends its statements: `\'` stays in
/// the string (#187), `#` comments hold `;` and `'` (#188), and `--x` is
/// code (#189).
#[test]
fn scripts_split_as_the_server_reads_them() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let script = "drop table if exists pythia_script;\n\
                  create table pythia_script (id int, note varchar(50));\n\
                  # the notes; it's a test\n\
                  insert into pythia_script values (1, 'it\\'s; here'), (2, \"a \\\"b\\\"; c\");\n\
                  select 1 --1 as two";
    let statements: Vec<&str> = pythia_plsql::split_statements_in(script, Dialect::MYSQL.syntax())
        .iter()
        .map(|s| s.text(script))
        .collect();
    assert_eq!(statements.len(), 4, "{statements:?}");
    for statement in &statements[..3] {
        h.ok(statement);
    }
    assert_eq!(h.scalar(statements[3]).as_deref(), Some("2"));
    assert_eq!(
        h.scalar("select group_concat(note order by id separator '|') from pythia_script")
            .as_deref(),
        Some("it's; here|a \"b\"; c")
    );
    h.ok("drop table pythia_script");
}

/// A trigger and a procedure between `DELIMITER ;;` lines, as mysqldump
/// writes them (#203): their bodies stay whole, and nothing of them runs
/// on its own. While the binary log is on, a trigger takes SUPER (1419):
/// without it, the procedure goes on alone.
#[test]
fn delimiters_as_mysqldump_writes_them() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let script = "DROP TABLE IF EXISTS `fs_dump`;\n\
                  CREATE TABLE `fs_dump` (\n  `a` int DEFAULT NULL,\n  `b` varchar(20) DEFAULT NULL\n);\n\
                  DELIMITER ;;\n\
                  CREATE DEFINER=CURRENT_USER TRIGGER `fs_dump_t` BEFORE INSERT ON `fs_dump` \
                  FOR EACH ROW begin\n  set new.b = concat(new.b, ';;', new.a * 2);\nend ;;\n\
                  DELIMITER ;\n\
                  DROP PROCEDURE IF EXISTS `fs_dump_p`;\n\
                  DELIMITER ;;\n\
                  CREATE DEFINER=CURRENT_USER PROCEDURE `fs_dump_p`()\n\
                  begin\n  insert into fs_dump values (1, 'x');\n  insert into fs_dump values (2, 'y');\nend ;;\n\
                  DELIMITER ;\n\
                  select count(*) from fs_dump;\n\
                  call fs_dump_p();";
    let statements: Vec<&str> = pythia_plsql::split_statements_in(script, Dialect::MYSQL.syntax())
        .iter()
        .map(|s| s.text(script))
        .collect();
    assert_eq!(statements.len(), 7, "{statements:?}");
    h.ok(statements[0]);
    h.ok(statements[1]);
    let triggered = match h.run(0, statements[2]) {
        DbEvent::Failed { error, .. } if error.to_string().contains("1419") => false,
        DbEvent::Failed { error, .. } => panic!("{}: {error}", statements[2]),
        _ => true,
    };
    h.settle();
    for statement in &statements[3..5] {
        h.ok(statement);
    }
    assert_eq!(h.scalar(statements[5]).as_deref(), Some("0"));
    h.ok(statements[6]);
    assert_eq!(
        h.scalar("select group_concat(b order by a separator '|') from fs_dump")
            .as_deref(),
        Some(if triggered { "x;;2|y;;4" } else { "x|y" })
    );
    h.ok("drop procedure fs_dump_p");
    h.ok("drop table fs_dump");
}

/// A file as mysqldump writes it: its SETs, a table's partitions and its
/// trigger, written in `/*!NNNNN …*/`, run as the server's code (#216).
#[test]
fn executable_comments_as_mysqldump_writes_them() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let script = "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\n\
                  /*!50503 SET NAMES utf8mb4 */;\n\
                  /*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;\n\
                  /*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;\n\
                  DROP TABLE IF EXISTS `fs_exec`;\n\
                  /*!40101 SET @saved_cs_client     = @@character_set_client */;\n\
                  /*!50503 SET character_set_client = utf8mb4 */;\n\
                  CREATE TABLE `fs_exec` (\n  `a` int DEFAULT NULL,\n  `b` varchar(20) DEFAULT NULL\n\
                  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4\n\
                  /*!50100 PARTITION BY HASH (`a`)\nPARTITIONS 2 */;\n\
                  /*!40101 SET character_set_client = @saved_cs_client */;\n\
                  /*!50003 SET @saved_sql_mode       = @@sql_mode */ ;\n\
                  /*!50003 SET sql_mode              = 'STRICT_TRANS_TABLES' */ ;\n\
                  DELIMITER ;;\n\
                  /*!50003 CREATE*/ /*!50017 DEFINER=CURRENT_USER*/ /*!50003 TRIGGER `fs_exec_t` \
                  BEFORE INSERT ON `fs_exec` FOR EACH ROW begin\n  \
                  set new.b = concat(new.b, ';', new.a * 2);\nend */;;\n\
                  DELIMITER ;\n\
                  /*!50003 SET sql_mode              = @saved_sql_mode */ ;\n\
                  LOCK TABLES `fs_exec` WRITE;\n\
                  /*!40000 ALTER TABLE `fs_exec` DISABLE KEYS */;\n\
                  INSERT INTO `fs_exec` VALUES (1,'x'),(2,'y');\n\
                  /*!40000 ALTER TABLE `fs_exec` ENABLE KEYS */;\n\
                  UNLOCK TABLES;\n\
                  /*!40101 SET SQL_MODE=@OLD_SQL_MODE */;\n\
                  /*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;";
    let statements: Vec<&str> = pythia_plsql::split_statements_in(script, Dialect::MYSQL.syntax())
        .iter()
        .map(|s| s.text(script))
        .collect();
    assert_eq!(statements.len(), 20, "{statements:?}");
    let mut triggered = false;
    for statement in &statements {
        if !statement.contains("TRIGGER") {
            h.ok(statement);
            continue;
        }
        // While the binary log is on, a trigger takes SUPER (1419).
        triggered = match h.run(0, statement) {
            DbEvent::Failed { error, .. } if error.to_string().contains("1419") => false,
            DbEvent::Failed { error, .. } => panic!("{statement}: {error}"),
            _ => true,
        };
        h.settle();
    }
    assert!(h.scalar("select @OLD_SQL_MODE").is_some());
    assert_eq!(
        h.scalar(
            "select count(*) from information_schema.partitions \
              where table_schema = database() and table_name = 'fs_exec' \
                and partition_name is not null"
        )
        .as_deref(),
        Some("2")
    );
    assert_eq!(
        h.scalar("select group_concat(b order by a separator '|') from fs_exec")
            .as_deref(),
        Some(if triggered { "x;2|y;4" } else { "x|y" })
    );
    h.ok("drop table fs_exec");
}

/// Without a database in the address, the login statements pick it (the
/// profile's default schema): `Connected` reports the one they leave the
/// session in, then their failures (#103).
#[test]
fn connected_reports_the_database_after_the_login_statements() {
    let Some(mut params) = params() else {
        return;
    };
    let (server, database) = params.connect_string.rsplit_once('/').unwrap();
    let database = database.to_owned();
    params.connect_string = server.to_owned();
    params.init_sql = vec![format!("use {database}"), "select no_such_column".into()];
    let mut h = Harness::with(params);
    assert_eq!(h.schema, database);
    let DbEvent::Notice(notice) = h.next() else {
        panic!("expected the login script's failure")
    };
    assert!(notice.starts_with("Login script: ERROR 1054"), "{notice}");
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
    let DbEvent::QueryStarted { .. } = h.run(1, "select count(*) from tx_t") else {
        panic!("expected a query")
    };
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(
        rows[0][0].as_deref(),
        Some("0"),
        "the insert was rolled back"
    );
    assert!(!h.settle());

    // A failed change leaves no transaction behind; in one, it keeps it.
    h.session.send(DbRequest::Execute {
        id: 3,
        sql: "insert into tx_t values (5), (5)".into(),
        max_rows: 10,
        binds: vec![],
    });
    assert!(matches!(h.next_result(), DbEvent::Failed { .. }));
    assert!(!h.settle());
    assert!(h.ok("insert into tx_t values (5)"));
    h.session.send(DbRequest::Execute {
        id: 4,
        sql: "insert into tx_t values (5)".into(),
        max_rows: 10,
        binds: vec![],
    });
    assert!(matches!(h.next_result(), DbEvent::Failed { .. }));
    assert!(h.settle());
    h.session.send(DbRequest::Rollback);
    assert!(!h.settle());

    h.session.send(DbRequest::SetAutocommit(true));
    let DbEvent::Executed { summary, .. } = h.run(2, "insert into tx_t values (2), (3)") else {
        panic!("expected Executed")
    };
    assert_eq!(summary.rows_affected, Some(2));
    assert_eq!(summary.to_string(), "INSERT: 2 rows affected");
    assert!(!h.settle(), "autocommit committed the insert");
    h.ok("drop table tx_t");
}

/// Rows affected are the rows an UPDATE matched, as on Oracle, also when it
/// changes nothing: editing a result relies on it to find conflicts.
#[test]
fn updates_count_matched_rows() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists found_t");
    h.ok("create table found_t (id int primary key, x int)");
    h.ok("insert into found_t values (1, 10), (2, 20)");
    let DbEvent::Executed { summary, .. } = h.run(1, "update found_t set x = x") else {
        panic!("expected Executed")
    };
    assert_eq!(summary.rows_affected, Some(2));
    h.settle();
    h.ok("drop table found_t");
}

/// Like Oracle's: a query neither pins a snapshot nor holds locks.
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
    // A million rows; only ten are fetched.
    let big = "with recursive r (n) as (select 1 union all select n + 1 from r where n < 1000) \
               select a.n * 1000 + b.n from r a, r b";
    h.session.send(DbRequest::Execute {
        id: 1,
        sql: big.into(),
        max_rows: 10,
        binds: vec![],
    });
    assert!(matches!(h.next_result(), DbEvent::QueryStarted { .. }));
    assert!(matches!(h.next_result(), DbEvent::Rows { more: true, .. }));

    let started = Instant::now();
    let DbEvent::QueryStarted { .. } = h.run(2, "select 'next'") else {
        panic!("expected the next query to run")
    };
    let DbEvent::Rows { rows, .. } = h.next_result() else {
        panic!("expected Rows")
    };
    assert_eq!(rows[0][0].as_deref(), Some("next"));
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

    // The kill didn't hit a later statement.
    for i in 0..3 {
        h.ok(&format!("select {i}"));
    }
}

#[test]
fn cancel_interrupts_a_running_statement() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.session.send(DbRequest::Execute {
        id: 1,
        // A billion joined rows. (Killed, SLEEP and BENCHMARK just return.)
        sql: "with recursive r (n) as (select 1 union all select n + 1 from r where n < 1000) \
              select count(*) from r a, r b, r c"
            .into(),
        max_rows: 10,
        binds: vec![],
    });
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    h.session.cancel();
    // The server sends the column before it computes the row.
    let error = loop {
        match h.next_result() {
            DbEvent::QueryStarted { .. } => {}
            DbEvent::Failed { error, .. } => break error,
            other => panic!("expected Failed, got {other:?}"),
        }
    };
    assert_eq!(error.code, Some(1317), "{}", error.message);
    assert!(started.elapsed() < Duration::from_secs(5));
    h.ok("select 1");
}

/// A TCP relay to the server that can be frozen: then nothing gets
/// through, in either direction, on its connections or on new ones, as
/// when the network goes quiet.
struct Relay {
    port: u16,
    frozen: Arc<AtomicBool>,
}

impl Relay {
    fn to(server: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let frozen = Arc::new(AtomicBool::new(false));
        let server = server.to_owned();
        let relay = frozen.clone();
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let (Ok(client), Ok(upstream)) = (client, TcpStream::connect(&server)) else {
                    continue;
                };
                let back = (upstream.try_clone().unwrap(), client.try_clone().unwrap());
                for (from, to) in [(client, upstream), back] {
                    let frozen = relay.clone();
                    std::thread::spawn(move || pump(from, to, &frozen));
                }
            }
        });
        Self { port, frozen }
    }

    fn freeze(&self, frozen: bool) {
        self.frozen.store(frozen, Ordering::SeqCst);
    }
}

fn pump(mut from: TcpStream, mut to: TcpStream, frozen: &AtomicBool) {
    let thaw = || {
        while frozen.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let mut buffer = [0; 16 * 1024];
    loop {
        thaw();
        let n = match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        thaw();
        if to.write_all(&buffer[..n]).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

/// Break returns at once while the network is gone (#207): it waited for
/// KILL QUERY's connection of its own, which waited for the server. Once
/// the network is back, the kill goes through.
#[test]
fn cancel_never_waits_for_the_network() {
    let Some(params) = params() else {
        return;
    };
    let (server, database) = params.connect_string.split_once('/').unwrap();
    let relay = Relay::to(server);
    let mut h = Harness::with(ConnectParams {
        connect_string: format!("127.0.0.1:{}/{database}", relay.port),
        ..params.clone()
    });
    h.session.send(DbRequest::Execute {
        id: 1,
        sql: "select sleep(5)".into(),
        max_rows: 10,
        binds: vec![],
    });
    std::thread::sleep(Duration::from_millis(300));
    relay.freeze(true);
    let thaw = {
        let frozen = relay.frozen.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            frozen.store(false, Ordering::SeqCst);
        })
    };
    let started = Instant::now();
    h.session.cancel();
    let took = started.elapsed();
    assert!(took < Duration::from_millis(100), "Break took {took:?}");
    thaw.join().unwrap();
    // A killed SLEEP returns 1.
    let rows = loop {
        match h.next_result() {
            DbEvent::QueryStarted { .. } => {}
            DbEvent::Rows { rows, .. } => break rows,
            other => panic!("expected Rows, got {other:?}"),
        }
    };
    assert_eq!(rows[0][0].as_deref(), Some("1"));
    assert!(started.elapsed() < Duration::from_secs(4));
    h.ok("select 1");
}

/// A statement larger than max_allowed_packet fails, and the session goes
/// on with its transaction: the driver refuses it before sending (#205).
#[test]
fn statements_too_large_keep_the_session() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists fx_packet_t");
    h.ok("create table fx_packet_t (id int primary key)");
    let connection = h.scalar("select connection_id()");
    let max: usize = h
        .scalar("select @@max_allowed_packet")
        .unwrap()
        .parse()
        .unwrap();
    assert!(h.ok("insert into fx_packet_t values (1)"));
    let DbEvent::Failed { error, .. } = h.run(1, &format!("select '{}'", "x".repeat(max))) else {
        panic!("expected Failed")
    };
    assert!(!error.is_connection_lost(), "{error}");
    assert!(error.message.contains("max_allowed_packet"), "{error}");
    assert!(h.settle(), "the insert is still pending");
    assert_eq!(h.scalar("select connection_id()"), connection);
    assert_eq!(
        h.scalar("select count(*) from fx_packet_t").as_deref(),
        Some("1")
    );
    h.session.send(DbRequest::Rollback);
    assert!(!h.settle());
    h.ok("drop table fx_packet_t");
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
        "This tool isn't available for MySQL connections"
    );
}

#[test]
fn warnings_become_notices() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    h.ok("drop table if exists warn_t");
    h.ok("create table warn_t (a tinyint)");
    h.session.send(DbRequest::Execute {
        id: 1,
        sql: "insert ignore into warn_t values (1000)".into(),
        max_rows: 10,
        binds: vec![],
    });
    let mut notices = Vec::new();
    loop {
        match h.next() {
            DbEvent::Notice(text) => notices.push(text),
            DbEvent::Transaction { .. } => break,
            _ => {}
        }
    }
    assert!(
        notices.iter().any(|n| n.starts_with("Warning 1264: ")),
        "{notices:?}"
    );
    h.session.send(DbRequest::Rollback);
    h.settle();
    h.ok("drop table warn_t");
}

/// A query's warnings show after its rows (#210), with binds too.
#[test]
fn query_warnings_become_notices() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let events = |h: &mut Harness, sql: &str, binds: Vec<(String, Option<String>)>| {
        h.session.send(DbRequest::Execute {
            id: 1,
            sql: sql.into(),
            max_rows: 10,
            binds,
        });
        let mut seen = Vec::new();
        loop {
            match h.next() {
                DbEvent::Rows { .. } => seen.push("(rows)".to_owned()),
                DbEvent::Notice(n) => seen.push(n),
                DbEvent::Failed { error, .. } => panic!("{sql}: {error}"),
                DbEvent::Transaction { .. } => return seen,
                _ => {}
            }
        }
    };
    assert_eq!(
        events(&mut h, "select 1/0", Vec::new()),
        ["(rows)", "Warning 1365: Division by 0"]
    );
    assert_eq!(
        events(
            &mut h,
            "select :a / 0",
            vec![("a".into(), Some("1".into()))]
        ),
        ["(rows)", "Warning 1365: Division by 0"]
    );
    // None after a query without them.
    assert_eq!(events(&mut h, "select 1", Vec::new()), ["(rows)"]);
}

#[test]
fn dictionary() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let schema = h.schema.clone();
    h.ok("drop table if exists meta_emp");
    h.ok("drop table if exists meta_dept");
    h.ok("drop view if exists meta_v");
    h.ok("drop procedure if exists meta_p");
    h.ok("create table meta_dept (deptno int primary key, name varchar(30) not null comment 'Department')");
    h.ok("create table meta_emp (empno int primary key, deptno int, \
         constraint meta_emp_fk foreign key (deptno) references meta_dept (deptno), \
         index meta_emp_dept (deptno))");
    h.ok("create view meta_v as select e.empno, d.name from meta_emp e join meta_dept d using (deptno)");
    h.ok("create procedure meta_p(in p int) begin select p; end");

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

    let MetaResult::Objects(views) = h.meta(MetaQuery::Objects {
        owner: schema.clone(),
        object_type: ObjectType::View,
        filter: None,
    }) else {
        panic!("expected objects")
    };
    assert!(
        views
            .iter()
            .any(|o| o.name == "meta_v" && o.object_type == ObjectType::View)
    );

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
    assert_eq!(fk[3].as_deref(), Some("meta_dept"));

    let MetaResult::Table { rows, .. } = h.meta(MetaQuery::Details {
        owner: schema.clone(),
        name: "meta_emp".into(),
        detail: Detail::Indexes,
    }) else {
        panic!("expected a table")
    };
    assert!(
        rows.iter()
            .any(|r| r[0].as_deref() == Some("meta_emp_dept"))
    );

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
    let uses: Vec<_> = rows
        .iter()
        .map(|r| r[1].clone().unwrap_or_default())
        .collect();
    assert_eq!(uses, ["meta_dept", "meta_emp"]);

    let MetaResult::Text(ddl) = h.meta(MetaQuery::Ddl {
        owner: schema.clone(),
        name: "meta_emp".into(),
        object_type: ObjectType::Table,
    }) else {
        panic!("expected DDL")
    };
    assert!(ddl.starts_with("CREATE TABLE `meta_emp`"), "{ddl}");
    assert!(ddl.ends_with(';'));

    let MetaResult::Text(source) = h.meta(MetaQuery::Source {
        owner: schema.clone(),
        name: "meta_p".into(),
        object_type: ObjectType::Procedure,
    }) else {
        panic!("expected source")
    };
    assert!(source.contains("PROCEDURE `meta_p`"), "{source}");

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
    assert_eq!(found[0].owner, schema, "the current schema comes first");

    let MetaResult::Table { columns, rows } = h.meta(MetaQuery::ExplainPlan {
        sql: "select * from meta_emp e join meta_dept d on d.deptno = e.deptno".into(),
    }) else {
        panic!("expected a plan")
    };
    assert_eq!(columns[0].name, "ID");
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .any(|r| r[4].as_deref() == Some("d") || r[4].as_deref() == Some("e")),
        "{rows:?}"
    );

    h.ok("drop procedure meta_p");
    h.ok("drop view meta_v");
    h.ok("drop table meta_emp");
    h.ok("drop table meta_dept");
}

#[test]
fn accounts() {
    let Some(mut h) = Harness::connect() else {
        return;
    };
    let accounts = |h: &mut Harness, object_type| {
        let MetaResult::Accounts(accounts) = h.meta(MetaQuery::Accounts(object_type)) else {
            panic!("expected accounts")
        };
        accounts
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
    };
    let me = h.scalar("select current_user()").expect("the current user");
    let users = accounts(&mut h, ObjectType::User);
    assert!(users.contains(&me), "{users:?}");
    if users.len() == 1 {
        // No access to mysql.user: the session's own user, and its roles.
        accounts(&mut h, ObjectType::Role);
        return;
    }
    // A role is listed by its name, not as a user.
    h.ok("drop role if exists meta_role");
    h.ok("create role meta_role");
    let roles = accounts(&mut h, ObjectType::Role);
    assert!(roles.contains(&"meta_role".to_owned()), "{roles:?}");
    let users = accounts(&mut h, ObjectType::User);
    assert!(
        !users.iter().any(|u| u.starts_with("meta_role")),
        "{users:?}"
    );
    h.ok("drop role meta_role");
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

/// A certificate that isn't trusted (the test server's is self-signed)
/// says so and how to connect anyway, instead of "Lost connection to the
/// MySQL server: IO error: `invalid peer certificate: UnknownIssuer'"
/// (#211).
#[test]
fn untrusted_certificates_say_so() {
    let Some(params) = params() else {
        return;
    };
    let with = |options: &str| ConnectParams {
        connect_string: format!("{}?{options}", params.connect_string),
        ..params.clone()
    };
    let error = connect_error(with("tls"));
    assert!(
        error
            .message
            .starts_with("The server's certificate isn't trusted (invalid peer certificate: "),
        "{error}"
    );
    assert!(error.message.contains("?tls=skip-verify"), "{error}");
    assert!(!error.is_connection_lost());
    let mut h = Harness::with(with("tls=skip-verify"));
    assert_eq!(h.scalar("select 1").as_deref(), Some("1"));
}

#[test]
fn through_an_ssh_tunnel() {
    let dir = std::env::temp_dir().join(format!("pythia-mysql-ssh-{}", std::process::id()));
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
    assert_eq!(other.scalar("select database()").as_deref(), Some("pythia"));

    // Break sends KILL QUERY over a connection of its own, through the tunnel.
    h.session.send(DbRequest::Execute {
        id: 1,
        sql: "with recursive r (n) as (select 1 union all select n + 1 from r where n < 1000) \
              select count(*) from r a, r b, r c"
            .into(),
        max_rows: 10,
        binds: vec![],
    });
    std::thread::sleep(Duration::from_millis(500));
    h.session.cancel();
    let error = loop {
        match h.next_result() {
            DbEvent::QueryStarted { .. } => {}
            DbEvent::Failed { error, .. } => break error,
            other => panic!("expected Failed, got {other:?}"),
        }
    };
    assert_eq!(error.code, Some(1317), "{}", error.message);
    h.ok("select 1");

    // An address the SSH server can't reach: the tunnel says why.
    let unreachable = ConnectParams {
        connect_string: "nosuchhost.invalid:3306/pythia".into(),
        ..params
    };
    let error = connect_error(unreachable);
    assert!(
        error
            .message
            .contains("the server couldn't connect to nosuchhost.invalid:3306"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
