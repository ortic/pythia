//! Compare → apply sync script → re-compare against a real database.
//!
//! Skipped unless `PYTHIA_TEST_DB_DBA=system/password@connect_string` is set
//! (a DBA user that may create and drop the PYTHIA_CMP_A / PYTHIA_CMP_B schemas, here
//! with the checkout's tag).

use pythia_compare::data::{self, DataOptions, DataSource};
use pythia_compare::schema::{self, Options, Status};
mod common;

use common::{connect, own_schemas, run, setup_schemas, teardown_schemas};

fn describe(diff: &schema::SchemaDiff) -> String {
    diff.objects
        .iter()
        .filter(|o| o.status != Status::Equal)
        .map(|o| {
            format!(
                "{:?} {:?} {}\n  statements: {:?}\n  source:\n{}\n  target:\n{}\n",
                o.status,
                o.key.object_type,
                o.key.name,
                o.statements.iter().map(|s| &s.sql).collect::<Vec<_>>(),
                o.source_ddl.as_deref().unwrap_or("-"),
                o.target_ddl.as_deref().unwrap_or("-")
            )
        })
        .collect()
}

#[test]
fn schema_and_data_roundtrip() {
    let Some(conn) = connect() else {
        eprintln!("PYTHIA_TEST_DB_DBA not set, skipping");
        return;
    };
    let [a, b] = own_schemas();
    let schemas = [a.as_str(), b.as_str()];
    setup_schemas(&conn, schemas);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let options = Options {
            include_drops: true,
            ..Options::default()
        };
        let source = schema::collect(&conn, &a, &options).expect("collect source");
        let target = schema::collect(&conn, &b, &options).expect("collect target");
        let diff = schema::compare(&source, &target, &options);
        assert!(diff.errors.is_empty(), "{:?}", diff.errors);

        let status = |name: &str| {
            diff.objects
                .iter()
                .find(|o| o.key.name == name)
                .map(|o| o.status)
        };
        assert_eq!(status("DEPT"), Some(Status::Equal), "{}", describe(&diff));
        assert_eq!(status("EMP"), Some(Status::Different));
        assert_eq!(status("NEW_T"), Some(Status::OnlyInSource));
        assert_eq!(status("OLD_T"), Some(Status::OnlyInTarget));
        assert_eq!(status("S1"), Some(Status::Different));
        assert_eq!(status("F"), Some(Status::Equal));
        assert_eq!(status("P_A"), Some(Status::OnlyInSource));
        assert_eq!(status("P_B"), Some(Status::OnlyInTarget));
        assert_eq!(status("TRG"), Some(Status::OnlyInSource));
        assert_eq!(status("SYN"), Some(Status::OnlyInSource));

        let script = diff.script(&format!("{a} → {b}"));
        eprintln!("{script}");
        run(&conn, &script);
        run(&conn, "alter session set current_schema = system;");

        let target = schema::collect(&conn, &b, &options).expect("collect target again");
        let again = schema::compare(&source, &target, &options);
        assert!(
            again.is_equal(),
            "not equal after sync:\n{}",
            describe(&again)
        );

        // Data: DEPT rows differ (changed, missing, extra).
        let src = DataSource::Table {
            schema: a.clone(),
            table: "DEPT".into(),
        };
        let tgt = DataSource::Table {
            schema: b.clone(),
            table: "DEPT".into(),
        };
        let options = DataOptions {
            chunk_size: 2,
            ..DataOptions::default()
        };
        let mut progress_calls = 0;
        let d = data::compare(&conn, &src, &conn, &tgt, options.clone(), |_, _| {
            progress_calls += 1
        })
        .expect("data compare");
        assert_eq!(
            (d.equal, d.changed, d.only_in_source, d.only_in_target),
            (1, 1, 1, 1),
            "{d:?}"
        );
        assert!(progress_calls > 0);
        let script = d.script("data");
        eprintln!("{script}");
        run(&conn, &script);
        run(&conn, "commit;");
        let options_again = || options.clone();
        let d = data::compare(&conn, &src, &conn, &tgt, options.clone(), |_, _| {})
            .expect("data compare again");
        assert!(d.is_equal(), "{d:?}");
        assert_eq!(d.equal, 3);
        // A filter and a query that end with a `--` comment.
        let query = DataSource::Query(format!("select * from {a}.dept -- all of them"));
        let filtered = DataOptions {
            key_columns: vec!["ID".into()],
            filter: "id > 0 -- every one".into(),
            ..options_again()
        };
        let d = data::compare(&conn, &query, &conn, &tgt, filtered, |_, _| {})
            .expect("data compare with comments");
        assert_eq!(d.equal, 3, "{d:?}");
        // A filter that is all comment is no filter.
        let commented = DataOptions {
            key_columns: vec!["ID".into()],
            filter: "-- id > 1".into(),
            ..options_again()
        };
        let d = data::compare(&conn, &query, &conn, &tgt, commented, |_, _| {})
            .expect("data compare with a commented-out filter");
        assert_eq!(d.equal, 3, "{d:?}");
    }));
    teardown_schemas(&conn, schemas);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// A table whose columns are called like reserved words, read from the
/// database, is created again by the sync statements, and its missing
/// columns are added to another (with `PYTHIA_TEST_DB`, no DBA needed).
#[test]
fn reserved_words_sync() {
    use pythia_compare::table::{self, CreateOptions};
    let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (user, password) = credentials.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, password, connect_string).unwrap();
    let tables = ["R65_WORDS", "R65_WORDS_COPY", "R65_WORDS_FEW"];
    for t in tables {
        common::try_run(&conn, &format!("drop table {t} purge"));
    }
    run(
        &conn,
        "create table r65_words (id number primary key, \"RESOURCE\" number, \
           \"RENAME\" number, \"EXCEPT\" number, \"MLSLABEL\" number, \"SUCCESSFUL\" number);
         create table r65_words_few (id number primary key);",
    );
    let owner = user.to_ascii_uppercase();
    let defs = table::load(&conn, &owner, Some("R65_WORDS")).unwrap();
    let mut def = defs["R65_WORDS"].clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        def.name = "R65_WORDS_COPY".into();
        for statement in table::create(&def, &CreateOptions::default()) {
            run(&conn, &statement.sql);
        }
        let few = table::load(&conn, &owner, Some("R65_WORDS_FEW")).unwrap();
        def.name = "R65_WORDS_FEW".into();
        for statement in table::diff(&def, &few["R65_WORDS_FEW"], false).statements {
            if statement.enabled {
                run(&conn, &statement.sql);
            }
        }
        for t in ["R65_WORDS_COPY", "R65_WORDS_FEW"] {
            let columns: i64 = conn
                .query_row_as(
                    "select count(*) from user_tab_columns where table_name = :1 \
                       and column_name in ('RESOURCE', 'RENAME', 'EXCEPT', 'MLSLABEL', 'SUCCESSFUL')",
                    &[&t],
                )
                .unwrap();
            assert_eq!(columns, 5, "{t}");
        }
    }));
    for t in tables {
        common::try_run(&conn, &format!("drop table {t} purge"));
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// A table's invisible columns are compared and synced, which `select *`
/// left out (#444); its virtual ones are neither, as the sync statements
/// can't write them (#443): run, they make the target the source.
#[test]
fn invisible_and_virtual_columns_sync() {
    let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (user, password) = credentials.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, password, connect_string).unwrap();
    let tables = ["FD_COLS_S", "FD_COLS_T"];
    for t in tables {
        common::try_run(&conn, &format!("drop table {t} purge"));
    }
    for t in tables {
        run(
            &conn,
            &format!(
                "create table {t} (id number primary key, n number, \
                   hidden_n number invisible, twice as (n * 2) virtual)"
            ),
        );
    }
    run(
        &conn,
        "insert into fd_cols_s (id, n, hidden_n) values (1, 1, 10);
         insert into fd_cols_s (id, n, hidden_n) values (2, 2, 20);
         insert into fd_cols_t (id, n, hidden_n) values (1, 1, 99);",
    );
    let owner = user.to_ascii_uppercase();
    let table = |name: &str| DataSource::Table {
        schema: owner.clone(),
        table: name.into(),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let diff = data::compare(
            &conn,
            &table("FD_COLS_S"),
            &conn,
            &table("FD_COLS_T"),
            DataOptions::default(),
            |_, _| {},
        )
        .unwrap();
        assert_eq!((diff.changed, diff.only_in_source), (1, 1));
        assert!(
            diff.statements.iter().all(|s| !s.contains("twice")),
            "{:?}",
            diff.statements
        );
        for statement in &diff.statements {
            run(&conn, statement);
        }
        let same: i64 = conn
            .query_row_as(
                "select count(*) from (select id, n, hidden_n, twice from fd_cols_s \
                   minus select id, n, hidden_n, twice from fd_cols_t)",
                &[],
            )
            .unwrap();
        assert_eq!(same, 0);
    }));
    for t in tables {
        common::try_run(&conn, &format!("drop table {t} purge"));
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// A table's virtual, invisible, DEFAULT ON NULL and identity ALWAYS
/// columns and its NVARCHAR2 and NCHAR columns are made again as they are
/// by a copy and by the sync statements (#458, #459): a virtual column was one
/// with its expression for a default (ORA-00904), an invisible one left
/// out, ON NULL lost, the identity made BY DEFAULT ON NULL, and
/// `NVARCHAR2(10 CHAR)` refused (ORA-00907).
#[test]
fn generated_and_national_columns_sync() {
    use pythia_compare::table::{self, CreateOptions};
    let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (user, password) = credentials.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, password, connect_string).unwrap();
    let tables = ["FD_GEN_S", "FD_GEN_COPY", "FD_GEN_FEW"];
    for t in tables {
        common::try_run(&conn, &format!("drop table {t} purge"));
    }
    run(
        &conn,
        "create table fd_gen_s (id number generated always as identity, n number, \
           twice number generated always as (n * 2) virtual, \
           hidden_n number invisible, d number default on null 5 not null, \
           nv nvarchar2(10), nc nchar(5));
         create table fd_gen_few (other number);",
    );
    let owner = user.to_ascii_uppercase();
    let defs = table::load(&conn, &owner, Some("FD_GEN_S")).unwrap();
    let source = defs["FD_GEN_S"].clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut copy = source.clone();
        copy.name = "FD_GEN_COPY".into();
        for statement in table::create(&copy, &CreateOptions::default()) {
            run(&conn, &statement.sql);
        }
        let made = table::load(&conn, &owner, Some("FD_GEN_COPY")).unwrap();
        let columns = |def: &table::TableDef| def.columns.clone();
        assert_eq!(columns(&made["FD_GEN_COPY"]), columns(&source));
        let few = table::load(&conn, &owner, Some("FD_GEN_FEW")).unwrap();
        let mut target = source.clone();
        target.name = "FD_GEN_FEW".into();
        let diff = table::diff(&target, &few["FD_GEN_FEW"], false);
        for statement in diff.statements.iter().filter(|s| s.enabled) {
            run(&conn, &statement.sql);
        }
        let added = table::load(&conn, &owner, Some("FD_GEN_FEW")).unwrap();
        let mut expected = vec![few["FD_GEN_FEW"].columns[0].clone()];
        expected.extend(columns(&source));
        assert_eq!(columns(&added["FD_GEN_FEW"]), expected);
        // Equal tables differ in nothing.
        let again = table::diff(&made["FD_GEN_COPY"], &made["FD_GEN_COPY"], false);
        assert!(
            again.statements.is_empty() && again.notes.is_empty(),
            "{again:?}"
        );
    }));
    for t in tables {
        common::try_run(&conn, &format!("drop table {t} purge"));
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// A table's bitmap, reverse, compressed, invisible, local,
/// function-based and descending indexes are made again as they are by
/// the sync statements, and equal ones differ in nothing (#467): a bitmap
/// index was made a B-tree, the others plain or not compared at all.
#[test]
fn index_kinds_sync() {
    use pythia_compare::table;
    let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (user, password) = credentials.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, password, connect_string).unwrap();
    let tables = ["FD_IX_S", "FD_IX_T"];
    for t in tables {
        common::try_run(&conn, &format!("drop table {t} purge"));
    }
    for t in tables {
        run(
            &conn,
            &format!(
                "create table {t} (a number, b number, name varchar2(20)) partition by range (a) \
                   (partition p1 values less than (10), partition p2 values less than (maxvalue))"
            ),
        );
    }
    run(
        &conn,
        "create bitmap index fd_ix_s_bm on fd_ix_s (b) local;
         create index fd_ix_s_rev on fd_ix_s (a, b) compress 1 reverse invisible;
         create index fd_ix_s_fn on fd_ix_s (upper(name));
         create index fd_ix_s_desc on fd_ix_s (b desc, a);",
    );
    let owner = user.to_ascii_uppercase();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let source = table::load(&conn, &owner, Some("FD_IX_S")).unwrap()["FD_IX_S"].clone();
        assert_eq!(source.indexes.len(), 4, "{:?}", source.indexes);
        let target = table::load(&conn, &owner, Some("FD_IX_T")).unwrap()["FD_IX_T"].clone();
        let mut renamed = source.clone();
        renamed.name = "FD_IX_T".into();
        for index in &mut renamed.indexes {
            index.name = index.name.replace("FD_IX_S", "FD_IX_T");
        }
        for statement in table::diff(&renamed, &target, false).statements {
            if statement.enabled {
                run(&conn, &statement.sql);
            }
        }
        let made = table::load(&conn, &owner, Some("FD_IX_T")).unwrap()["FD_IX_T"].clone();
        let kinds = |def: &table::TableDef| -> Vec<(bool, Vec<String>, Vec<String>)> {
            def.indexes
                .iter()
                .map(|i| (i.kind.bitmap, i.kind.attributes.clone(), i.columns.clone()))
                .collect()
        };
        assert_eq!(kinds(&made), kinds(&source));
        let again = table::diff(&renamed, &made, false);
        assert!(again.statements.is_empty(), "{:?}", again.statements);
    }));
    for t in tables {
        common::try_run(&conn, &format!("drop table {t} purge"));
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
