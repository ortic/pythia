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
