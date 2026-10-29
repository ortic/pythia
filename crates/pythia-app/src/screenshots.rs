//! Renders the real app offscreen (wgpu) into PNGs for visual review.
//!
//! Skipped unless `PYTHIA_SCREENSHOT_DIR` is set. With `PYTHIA_TEST_DB`
//! (`user/password@connect_string`) the scenarios connect to a database:
//!
//! ```text
//! PYTHIA_SCREENSHOT_DIR=/tmp/shots PYTHIA_TEST_DB=pythia/pythia@localhost:1521/FREEPDB1 \
//!   cargo test -p pythia-app screenshots -- --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pythia_config::{ConnectionProfile, Privilege};

use crate::app::PythiaApp;
use crate::documents::{ObjectRef, OpenRequest};

fn out_dir() -> Option<PathBuf> {
    std::env::var_os("PYTHIA_SCREENSHOT_DIR").map(PathBuf::from)
}

/// How many UI tests run side by side. Each holds a few database sessions,
/// and Oracle Free has 200 processes (ORA-12516 beyond).
const UI_SLOTS: usize = 4;

/// UI tests run side by side, [`UI_SLOTS`] at a time (#139). A test on
/// state the others see (the debug scenario, the schema's invalid objects)
/// runs alone, and so does every test that renders or uses OCI sessions
/// (the thick driver): many wgpu renderers and OCI sessions in one test
/// process crash intermittently (SIGSEGV) when they run in parallel.
static UI_LOCK: UiLock = UiLock {
    state: std::sync::Mutex::new(UiState {
        shared: 0,
        alone: false,
        waiting_alone: 0,
    }),
    changed: std::sync::Condvar::new(),
};

struct UiLock {
    state: std::sync::Mutex<UiState>,
    changed: std::sync::Condvar,
}

struct UiState {
    /// Tests running side by side.
    shared: usize,
    /// A test running alone.
    alone: bool,
    /// Tests waiting to run alone: no new ones start beside the others
    /// meanwhile.
    waiting_alone: usize,
}

impl UiLock {
    fn state(&self) -> std::sync::MutexGuard<'_, UiState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait_while(&self, busy: impl Fn(&UiState) -> bool) -> std::sync::MutexGuard<'_, UiState> {
        let mut state = self.state();
        while busy(&state) {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state
    }

    fn shared(&'static self) -> UiGuard {
        let mut state = self.wait_while(|s| s.alone || s.waiting_alone > 0 || s.shared >= UI_SLOTS);
        state.shared += 1;
        UiGuard { alone: false }
    }

    fn alone(&'static self) -> UiGuard {
        self.state().waiting_alone += 1;
        let mut state = self.wait_while(|s| s.alone || s.shared > 0);
        state.waiting_alone -= 1;
        state.alone = true;
        UiGuard { alone: true }
    }
}

/// A test's place in [`UI_LOCK`], given back when the test ends (also when
/// it fails).
struct UiGuard {
    alone: bool,
}

impl Drop for UiGuard {
    fn drop(&mut self) {
        let mut state = UI_LOCK.state();
        if self.alone {
            state.alone = false;
        } else {
            state.shared -= 1;
        }
        UI_LOCK.changed.notify_all();
    }
}

/// A harness holding its place in the UI lock for the test's duration.
struct Ui {
    harness: Harness<'static, PythiaApp>,
    lock: Option<UiGuard>,
}

impl Ui {
    /// Runs the rest of the test with no other UI test beside it: for
    /// tests on state the others see or change.
    fn alone(mut self) -> Self {
        if !self.lock.as_ref().is_some_and(|l| l.alone) {
            self.lock = None;
            self.lock = Some(UI_LOCK.alone());
        }
        self
    }
}

impl std::ops::Deref for Ui {
    type Target = Harness<'static, PythiaApp>;
    fn deref(&self) -> &Self::Target {
        &self.harness
    }
}

impl std::ops::DerefMut for Ui {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.harness
    }
}

/// A harness with the renderer only when screenshots are written: tests
/// that drive the UI and query it also run where no GPU adapter is
/// available (the Linux CI runner).
fn harness(dark: bool) -> Ui {
    locked(dark, out_dir().is_some())
}

/// A harness in the light theme.
fn headless_harness() -> Ui {
    harness(false)
}

fn locked(dark: bool, render: bool) -> Ui {
    locked_sized(dark, render, egui::vec2(1440.0, 900.0))
}

/// A harness of `size` (logical pixels), e.g. a small window.
fn sized_harness(size: egui::Vec2) -> Ui {
    locked_sized(true, out_dir().is_some(), size)
}

fn locked_sized(dark: bool, render: bool, size: egui::Vec2) -> Ui {
    let lock = if render || pythia_db::THICK {
        UI_LOCK.alone()
    } else {
        UI_LOCK.shared()
    };
    Ui {
        harness: new_harness(dark, render, size),
        lock: Some(lock),
    }
}

fn new_harness(dark: bool, render: bool, size: egui::Vec2) -> Harness<'static, PythiaApp> {
    let builder = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(1.5);
    let builder = if render { builder.wgpu() } else { builder };
    let mut harness = builder.build_eframe(|cc| PythiaApp::new(cc, None));
    // Scenes start without the connect dialog Pythia opens on start.
    harness.state_mut().test_close_connect();
    harness.ctx.set_theme(if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    harness
}

fn save(harness: &mut Harness<'_, PythiaApp>, name: &str) {
    let Some(dir) = out_dir() else { return };
    std::fs::create_dir_all(&dir).unwrap();
    harness.run_steps(4);
    let image = harness.render().expect("render");
    let path = dir.join(format!("{name}.png"));
    image.save(&path).unwrap();
    eprintln!("wrote {}", path.display());
}

/// A file that exists on every platform but isn't a font (`/etc/hostname`
/// is missing on macOS and Windows), named after the test using it.
fn not_a_font(test: &str) -> String {
    let path = std::env::temp_dir().join(format!("pythia-{test}-{}.ttf", std::process::id()));
    std::fs::write(&path, "not a font").unwrap();
    path.to_string_lossy().into_owned()
}

/// Steps the app until `done` holds (DB events arrive on other threads).
fn wait_until(harness: &mut Harness<'_, PythiaApp>, what: &str, done: impl Fn(&PythiaApp) -> bool) {
    let start = Instant::now();
    while !done(harness.state()) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        harness.step();
        std::thread::sleep(Duration::from_millis(15));
    }
    harness.run_steps(3);
}

/// The test database's profile (`PYTHIA_TEST_DB`), with the demo schema in
/// place.
fn profile() -> Option<(ConnectionProfile, String)> {
    let spec = std::env::var("PYTHIA_TEST_DB").ok()?;
    crate::test_db::demo_schema();
    let (credentials, connect_string) = spec.rsplit_once('@')?;
    let (username, password) = credentials.split_once('/')?;
    Some((
        ConnectionProfile {
            name: "pythia@FREEPDB1".into(),
            username: username.into(),
            connect_string: connect_string.into(),
            privilege: Privilege::Normal,
            color: Some([0x3b, 0x82, 0xf6]),
            ..Default::default()
        },
        password.into(),
    ))
}

const QUERY: &str = "-- Salary overview per department
select d.department_name,
       count(*)                 as employees,
       round(avg(e.salary), 2)  as avg_salary,
       max(e.hire_date)         as last_hire
  from demo_employees e
  join demo_departments d on d.department_id = e.department_id
 group by d.department_name
 order by avg_salary desc;

select * from demo_employees where salary > 9000 order by salary desc;
";

#[test]
fn screenshots() {
    if out_dir().is_none() {
        eprintln!("PYTHIA_SCREENSHOT_DIR not set, skipping");
        return;
    }
    for dark in [true, false] {
        let suffix = if dark { "dark" } else { "light" };
        let mut h = harness(dark);
        h.run_steps(3);
        save(&mut h, &format!("01-start-{suffix}"));
        h.state_mut().test_settings(true);
        h.run_steps(4);
        save(&mut h, &format!("00-settings-formatter-{suffix}"));
        h.state_mut().test_close_settings();
        let sample = |name: &str, folder: &str, color: Option<[u8; 3]>, production: bool| {
            ConnectionProfile {
                name: name.into(),
                folder: folder.into(),
                username: "app".into(),
                connect_string: format!(
                    "{}.db.example.com:1521/SVC",
                    name.to_lowercase().replace(' ', "-")
                ),
                color,
                production,
                ..Default::default()
            }
        };
        h.state_mut().test_show_connections(vec![
            sample("Local dev", "", Some([0x22, 0xc5, 0x5e]), false),
            sample("Acme test", "acme", Some([0xea, 0xb3, 0x08]), false),
            sample("Acme prod", "acme", None, true),
            sample("Globex QA", "globex", Some([0xa8, 0x55, 0xf7]), false),
        ]);
        h.run_steps(4);
        save(&mut h, &format!("00-connect-{suffix}"));
        h.state_mut().test_close_connect();

        let Some((profile, password)) = profile() else {
            continue;
        };
        h.state_mut().test_connect(profile, password);
        wait_until(&mut h, "connection", |app| app.test_connected());

        h.state_mut().test_open(OpenRequest::Sql {
            connection: "pythia@FREEPDB1".into(),
            text: QUERY.into(),
            run: true,
        });
        wait_until(&mut h, "query", |app| !app.test_focused_busy());
        h.state_mut()
            .test_expand_browser(&[pythia_db::ObjectType::Table, pythia_db::ObjectType::Package]);
        wait_until(&mut h, "browser", |app| !app.test_browser_loading());
        save(&mut h, &format!("02-sql-window-{suffix}"));
        if dark {
            h.state_mut()
                .test_command(crate::commands::Command::ExplainPlan);
            wait_until(&mut h, "plan", |app| {
                app.test_sql_window_ref()
                    .is_some_and(|w| w.state.plan.as_ref().is_some_and(|p| p.result.is_some()))
            });
            save(&mut h, "13-plan-dark");
            h.state_mut()
                .test_command(crate::commands::Command::ExecuteWithStatistics);
            wait_until(&mut h, "statistics", |app| !app.test_focused_busy());
            save(&mut h, "14-statistics-dark");
        }

        let object = |name: &str, t| ObjectRef {
            owner: "PYTHIA".into(),
            name: name.into(),
            object_type: t,
        };
        h.state_mut().test_open(OpenRequest::Program {
            connection: "pythia@FREEPDB1".into(),
            object: object("DEMO_PKG", pythia_db::ObjectType::PackageBody),
        });
        wait_until(&mut h, "package source", |app| !app.test_focused_busy());
        save(&mut h, &format!("03-program-window-{suffix}"));

        h.state_mut().test_open(OpenRequest::Program {
            connection: "pythia@FREEPDB1".into(),
            object: object("DEMO_BROKEN", pythia_db::ObjectType::Procedure),
        });
        wait_until(&mut h, "procedure source", |app| !app.test_focused_busy());
        h.state_mut().test_execute_focused();
        wait_until(&mut h, "compile", |app| !app.test_focused_busy());
        save(&mut h, &format!("04-compile-errors-{suffix}"));

        h.state_mut().test_open(OpenRequest::Object {
            connection: "pythia@FREEPDB1".into(),
            object: object("DEMO_EMPLOYEES", pythia_db::ObjectType::Table),
        });
        wait_until(&mut h, "columns", |app| !app.test_focused_busy());
        save(&mut h, &format!("05-object-view-{suffix}"));

        h.state_mut().test_open(OpenRequest::Monitor {
            connection: "pythia@FREEPDB1".into(),
            start: crate::monitor_window::MonitorStart::Dashboard,
        });
        for _ in 0..3 {
            wait_until(&mut h, "monitor", |app| !app.test_focused_busy());
        }
        save(&mut h, &format!("07-monitor-{suffix}"));

        h.state_mut()
            .test_command(crate::commands::Command::CompareSchemas);
        h.run_steps(5);
        save(&mut h, &format!("09-compare-{suffix}"));

        h.state_mut()
            .test_command(crate::commands::Command::FindInSource);
        h.run_steps(3);
        h.state_mut().test_search_source("salary");
        wait_until(&mut h, "source search", |app| !app.test_focused_busy());
        save(&mut h, &format!("10-source-search-{suffix}"));

        if dark {
            h.state_mut()
                .test_command(crate::commands::Command::SearchData);
            h.run_steps(3);
            h.state_mut().test_search_data("Basel", false);
            wait_until(&mut h, "data search", |app| !app.test_focused_busy());
            let hits = h.state().test_data_hits();
            assert!(
                hits.contains(&"DEMO_DEPARTMENTS.LOCATION=1".to_owned()),
                "{hits:?}"
            );
            save(&mut h, "15-data-search-dark");
            h.state_mut().test_search_data("sal", true);
            wait_until(&mut h, "column search", |app| !app.test_focused_busy());
            let hits = h.state().test_data_hits();
            assert!(
                hits.contains(&"DEMO_EMPLOYEES.SALARY=0".to_owned()),
                "{hits:?}"
            );
            h.state_mut().test_open(OpenRequest::EditTable {
                connection: "pythia@FREEPDB1".into(),
                object: object("DEMO_EMPLOYEES", pythia_db::ObjectType::Table),
                tab: crate::table_editor::TableTab::Columns,
            });
            wait_until(&mut h, "table definition", |app| !app.test_focused_busy());
            save(&mut h, "16-table-editor-dark");
        }
    }
}

/// Edits a result in the grid, applies it, and checks that a row changed by
/// another session in the meantime is reported as a conflict.
#[test]
fn data_editing() {
    let Some((profile, password)) = profile() else {
        eprintln!("PYTHIA_TEST_DB not set, skipping");
        return;
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let conn = "pythia@FREEPDB1".to_string();
    let setup = "begin execute immediate 'drop table pythia_edit_t purge'; exception when others then null; end;\n/\n\
                 create table pythia_edit_t (id number primary key, name varchar2(20), hired date);\n\
                 insert into pythia_edit_t values (1, 'one', date '2020-01-01');\n\
                 insert into pythia_edit_t values (2, 'two', null);\n\
                 commit;\n";
    h.state_mut().test_open(OpenRequest::Sql {
        connection: conn.clone(),
        text: setup.into(),
        run: true,
    });
    wait_until(&mut h, "setup", |app| !app.test_focused_busy());
    h.state_mut().test_close_focused();

    h.state_mut().test_open(OpenRequest::Sql {
        connection: conn.clone(),
        text: "select * from pythia_edit_t order by id".into(),
        run: true,
    });
    wait_until(&mut h, "query", |app| !app.test_focused_busy());
    let grid_window = h.state().test_focused();

    let cx = |app: &mut PythiaApp| {
        let (id, settings, connection) = app.test_cx();
        (id, settings, connection)
    };
    let start_edit = |h: &mut Harness<'_, PythiaApp>| {
        let (id, settings, connection) = cx(h.state_mut());
        let app = h.state_mut();
        let cx = crate::documents::DocContext {
            id,
            settings: &settings,
            connected: true,
            fetch_size: 100,
            connection,
            catalog: None,
            catalog_revision: 0,
            connections: &[],
            debug: None,
            marks: None,
            privilege: None,
            dialect: Default::default(),
            read_only: false,
            production: false,
        };
        let effects = app.test_sql_window().start_editing(0, &cx);
        app.test_apply(effects);
    };
    start_edit(&mut h);
    wait_until(&mut h, "editable query", |app| !app.test_focused_busy());
    {
        let w = h.state_mut().test_sql_window();
        let r = &mut w.state.results[0];
        assert!(r.grid.is_editable(), "result should be editable");
        assert_eq!(r.columns.last().unwrap().type_name, "ROWID");
        let rows = r.rows.clone();
        r.grid.edit_cell(&rows, 0, 1, Some("uno".into()));
        r.grid
            .edit_cell(&rows, 1, 2, Some("2021-02-03 04:05:06".into()));
    }
    save(&mut h, "06-editing-dark");
    // The single record view shows the pending values and edits them too
    // (#141).
    h.state_mut().test_sql_window().state.results[0]
        .grid
        .record_view = true;
    h.run_steps(2);
    save(&mut h, "06-editing-record-dark");
    // Typing on a field replaces its value, Enter keeps it.
    h.get_by_label("uno").click();
    h.run();
    h.event(egui::Event::Text("eins".into()));
    h.run();
    h.key_press(egui::Key::Enter);
    h.run_steps(2);
    {
        let r = &mut h.state_mut().test_sql_window().state.results[0];
        let changes = r.grid.changes().unwrap();
        assert_eq!(changes.updates[&0][&1].as_deref(), Some("eins"));
        r.grid.record_view = false;
    }
    {
        let w = h.state_mut().test_sql_window();
        let r = &mut w.state.results[0];
        let rows = r.rows.clone();
        r.grid.open_value_editor(&rows, (0, 2));
    }
    h.run_steps(3);
    save(&mut h, "06-editing-date-dark");
    {
        let w = h.state_mut().test_sql_window();
        let r = &mut w.state.results[0];
        let rows = r.rows.clone();
        // Closing without Apply leaves the cell alone.
        r.grid.close_value_editor();
        assert_eq!(
            r.grid.changes().unwrap().updates[&0].get(&2),
            None,
            "{rows:?}"
        );
    }
    let apply = |h: &mut Harness<'_, PythiaApp>| {
        let (id, settings, connection) = cx(h.state_mut());
        let app = h.state_mut();
        let cx = crate::documents::DocContext {
            id,
            settings: &settings,
            connected: true,
            fetch_size: 100,
            connection,
            catalog: None,
            catalog_revision: 0,
            connections: &[],
            debug: None,
            marks: None,
            privilege: None,
            dialect: Default::default(),
            read_only: false,
            production: false,
        };
        let effects = app.test_sql_window().apply_changes(0, &cx);
        app.test_apply(effects);
    };
    apply(&mut h);
    wait_until(&mut h, "apply", |app| !app.test_focused_busy());
    {
        let w = h.state_mut().test_sql_window();
        let messages: Vec<String> = w.state.messages.iter().map(|m| m.text.clone()).collect();
        assert!(
            messages.iter().any(|m| m.contains("Changes applied")),
            "{messages:?}"
        );
        let r = &w.state.results[0];
        assert_eq!(r.rows[0][1].as_deref(), Some("eins"));
        assert_eq!(r.rows[1][2].as_deref(), Some("2021-02-03 04:05:06"));
        assert!(r.grid.changes().is_some_and(|c| c.is_empty()));
    }
    assert!(
        h.state().test_pending_tx(),
        "applied changes stay in the open transaction"
    );
    h.state_mut().test_commit();
    wait_until(&mut h, "commit", |app| !app.test_pending_tx());

    // Another session changes row 2 behind our back; editing it must conflict.
    h.state_mut().test_open(OpenRequest::Sql {
        connection: conn.clone(),
        text: "update pythia_edit_t set name = 'changed' where id = 2;\ncommit;".into(),
        run: true,
    });
    wait_until(&mut h, "concurrent update", |app| !app.test_focused_busy());
    h.state_mut().test_close_focused();
    h.state_mut().test_focus(grid_window);
    {
        let w = h.state_mut().test_sql_window();
        let r = &mut w.state.results[0];
        let rows = r.rows.clone();
        r.grid.edit_cell(&rows, 1, 1, Some("mine".into()));
    }
    apply(&mut h);
    wait_until(&mut h, "conflicting apply", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    let messages: Vec<String> = w.state.messages.iter().map(|m| m.text.clone()).collect();
    assert!(
        messages.iter().any(|m| m.starts_with("Conflict on row 2")),
        "{messages:?}"
    );
}

/// Edits MySQL results (`PYTHIA_TEST_MYSQL`, #101): rows are found by the
/// table's key, selected or not; values MySQL takes as they are (a date
/// for a DATETIME) aren't conflicts, rows changed by another session are;
/// tables without a key and grouped results stay read-only.
#[test]
fn mysql_data_editing() {
    let Some(spec) = std::env::var("PYTHIA_TEST_MYSQL").ok() else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (username, password) = credentials.split_once('/').unwrap();
    let profile = ConnectionProfile {
        name: "shop".into(),
        kind: pythia_config::DbKind::MySql,
        username: username.into(),
        connect_string: connect_string.into(),
        ..Default::default()
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password.into());
    wait_until(&mut h, "connection", |app| app.test_connected());
    let run = |h: &mut Harness<'_, PythiaApp>, sql: &str| {
        h.state_mut().test_open(OpenRequest::Sql {
            connection: "shop".into(),
            text: sql.into(),
            run: true,
        });
        wait_until(h, sql, |app| !app.test_focused_busy());
        h.state_mut().test_close_focused();
    };
    run(
        &mut h,
        "drop table if exists pythia_edit_t, pythia_edit_k, pythia_edit_n;\n\
         create table pythia_edit_t (id int auto_increment primary key, name varchar(20), \
           born datetime, doc json, price float);\n\
         insert into pythia_edit_t (name, born, doc, price) values \
           ('one', '2020-01-01 00:00:00', '{\"a\": 1}', 123456.789), ('two', null, null, 1.1);\n\
         create table pythia_edit_k (a int not null, b varchar(5) not null, v int, \
           unique key (a, b));\n\
         insert into pythia_edit_k values (1, 'x', 10), (1, 'y', 20);\n\
         create table pythia_edit_n (x int);\n\
         insert into pythia_edit_n values (1);\n\
         commit;\n",
    );
    let query = |h: &mut Harness<'_, PythiaApp>, sql: &str| {
        h.state_mut().test_open(OpenRequest::Sql {
            connection: "shop".into(),
            text: sql.into(),
            run: true,
        });
        wait_until(h, sql, |app| !app.test_focused_busy());
        h.state_mut()
            .test_on_sql_window(|w, cx| w.start_editing(0, cx));
        wait_until(h, "editing", |app| !app.test_focused_busy());
    };
    let apply = |h: &mut Harness<'_, PythiaApp>| {
        h.state_mut()
            .test_on_sql_window(|w, cx| w.apply_changes(0, cx));
        wait_until(h, "apply", |app| !app.test_focused_busy());
    };
    let messages = |h: &mut Harness<'_, PythiaApp>| -> Vec<String> {
        let w = h.state_mut().test_sql_window();
        w.state.messages.iter().map(|m| m.text.clone()).collect()
    };

    // The key isn't selected: Pythia adds it, read-only, at the end.
    query(
        &mut h,
        "select name, born, doc, price from pythia_edit_t order by id",
    );
    let grid_window = h.state().test_focused();
    {
        let w = h.state_mut().test_sql_window();
        let r = &mut w.state.results[0];
        assert!(
            r.grid.is_editable(),
            "{:?}",
            w.state.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["name", "born", "doc", "price", "pythia$id"]);
        assert_eq!(r.rows[0][3].as_deref(), Some("123457"));
        let rows = r.rows.clone();
        r.grid.edit_cell(&rows, 0, 0, Some(r"C:\new 'one'".into()));
        r.grid
            .edit_cell(&rows, 0, 2, Some(r#"{"a": [1, "\u00e9"]}"#.into()));
        // Shown rounded, still found.
        r.grid.edit_cell(&rows, 0, 3, Some("2.5".into()));
        r.grid.edit_cell(&rows, 1, 4, Some("99".into()));
        r.grid
            .edit_cell(&rows, 1, 1, Some("2021-02-03 04:05:06".into()));
    }
    save(&mut h, "mysql-editing");
    apply(&mut h);
    let texts = messages(&mut h);
    assert!(
        texts.iter().any(|m| m.contains("Changes applied")),
        "{texts:?}"
    );
    {
        let w = h.state_mut().test_sql_window();
        let r = &mut w.state.results[0];
        assert_eq!(r.rows[0][0].as_deref(), Some(r"C:\new 'one'"));
        assert_eq!(r.rows[0][1].as_deref(), Some("2020-01-01 00:00:00"));
        assert_eq!(r.rows[0][2].as_deref(), Some(r#"{"a": [1, "é"]}"#));
        assert_eq!(r.rows[0][3].as_deref(), Some("2.5"));
        assert_eq!(r.rows[1][1].as_deref(), Some("2021-02-03 04:05:06"));
        assert_eq!(
            r.rows[1][4].as_deref(),
            Some("2"),
            "the key can't be edited"
        );
        // A new row (the key from AUTO_INCREMENT), a deleted one, and the
        // same value written differently: its UPDATE changes nothing but
        // finds the row, which isn't a conflict.
        let rows = r.rows.clone();
        r.grid.add_row(&rows, r.columns.len());
        r.grid.edit_cell(&rows, 2, 0, Some("three".into()));
        r.grid.delete_row(&rows, 1);
        r.grid.edit_cell(&rows, 0, 1, Some("2020-01-01".into()));
    }
    apply(&mut h);
    let texts = messages(&mut h);
    assert!(!texts.iter().any(|m| m.contains("Conflict")), "{texts:?}");
    {
        let w = h.state_mut().test_sql_window();
        let r = &w.state.results[0];
        let names: Vec<Option<&str>> = r.rows.iter().map(|row| row[0].as_deref()).collect();
        assert_eq!(names, [Some(r"C:\new 'one'"), Some("three")]);
        assert_eq!(r.rows[0][1].as_deref(), Some("2020-01-01 00:00:00"));
    }
    h.state_mut().test_commit();
    wait_until(&mut h, "commit", |app| !app.test_pending_tx());

    // Another session changes the row behind our back.
    run(
        &mut h,
        "update pythia_edit_t set name = 'theirs' where name = 'three';\ncommit;",
    );
    h.state_mut().test_focus(grid_window);
    {
        let w = h.state_mut().test_sql_window();
        let r = &mut w.state.results[0];
        let rows = r.rows.clone();
        r.grid.edit_cell(&rows, 1, 0, Some("mine".into()));
    }
    apply(&mut h);
    let texts = messages(&mut h);
    assert!(
        texts.iter().any(|m| m.starts_with("Conflict on row 2")),
        "{texts:?}"
    );
    h.state_mut().test_rollback();
    wait_until(&mut h, "rollback", |app| !app.test_pending_tx());
    h.state_mut().test_close_focused();

    // A unique key on NOT NULL columns does, too.
    query(
        &mut h,
        "select v from pythia_edit_k k where a = 1 order by b",
    );
    {
        let w = h.state_mut().test_sql_window();
        let r = &mut w.state.results[0];
        let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["v", "pythia$a", "pythia$b"]);
        let rows = r.rows.clone();
        r.grid.edit_cell(&rows, 1, 0, Some("21".into()));
    }
    apply(&mut h);
    {
        let w = h.state_mut().test_sql_window();
        let values: Vec<Option<&str>> = w.state.results[0]
            .rows
            .iter()
            .map(|r| r[0].as_deref())
            .collect();
        assert_eq!(values, [Some("10"), Some("21")]);
    }
    h.state_mut().test_commit();
    wait_until(&mut h, "commit", |app| !app.test_pending_tx());
    h.state_mut().test_close_focused();

    // Without a key, rows can't be found again.
    query(&mut h, "select * from pythia_edit_n");
    let texts = messages(&mut h);
    assert!(
        texts
            .iter()
            .any(|m| m.starts_with("The rows of pythia_edit_n can't be edited")),
        "{texts:?}"
    );
    assert!(
        !h.state_mut().test_sql_window().state.results[0]
            .grid
            .is_editable()
    );
    h.state_mut().test_close_focused();

    // A count isn't a row of the table.
    query(&mut h, "select count(*) from pythia_edit_t");
    let texts = messages(&mut h);
    assert!(
        texts.iter().any(|m| m.contains("can't be edited")),
        "{texts:?}"
    );
    assert!(
        !h.state_mut().test_sql_window().state.results[0]
            .grid
            .is_editable()
    );
    h.state_mut().test_close_focused();

    run(
        &mut h,
        "drop table pythia_edit_t, pythia_edit_k, pythia_edit_n;",
    );
}

/// Types `.` after a table alias in a real editor and waits for the
/// completion popup, which needs a round trip to the dictionary.
#[test]
fn completion_popup() {
    let Some((profile, password)) = profile() else {
        eprintln!("PYTHIA_TEST_DB not set, skipping");
        return;
    };
    for dark in [true, false] {
        let mut h = harness(dark);
        h.state_mut()
            .test_connect(profile.clone(), password.clone());
        wait_until(&mut h, "connection", |app| app.test_connected());
        h.state_mut().test_open(OpenRequest::Sql {
            connection: "pythia@FREEPDB1".into(),
            text: "select e\n  from demo_employees e\n where e.department_id = 20".into(),
            run: false,
        });
        h.run_steps(3);
        h.state_mut().test_sql_window().editor.select(8..8);
        h.run_steps(3);
        h.event(egui::Event::Text(".".into()));
        wait_until(&mut h, "completion popup", |app| {
            app.test_sql_window_ref()
                .is_some_and(|w| w.completion_visible_with("SALARY"))
        });
        save(
            &mut h,
            &format!("08-completion-{}", if dark { "dark" } else { "light" }),
        );
    }
}

/// Imports a CSV with a decimal comma, European dates and NULLs into a new
/// table and checks the converted values in the database.
#[test]
fn import_csv() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("pythia-import-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("orders 2024.csv");
    std::fs::write(&path, "Order id;Customer;Amount;Order date\n1;Acme;1.234,50;31.12.2024\n2;;7;01.01.2025\n3;Globex;-0,5;15.06.2024\n").unwrap();

    let mut h = harness(true);
    h.state_mut()
        .test_connect(profile.clone(), password.clone());
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin execute immediate 'drop table pythia_import_t purge'; exception when others then null; end;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());

    h.state_mut().test_import(path, "pythia_import_t");
    wait_until(&mut h, "import", |app| !app.test_focused_busy());
    let message = h.state().test_import_message();
    assert!(
        matches!(&message, Some((false, m)) if m.starts_with("Imported 3 rows")),
        "{message:?}"
    );
    save(&mut h, "11-import-dark");

    let (cred, db) = std::env::var("PYTHIA_TEST_DB")
        .unwrap()
        .rsplit_once('@')
        .map(|(c, d)| (c.to_owned(), d.to_owned()))
        .unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, pw, &db).unwrap();
    let rows: Vec<(i64, Option<String>, f64, String)> = conn
        .query_as("select order_id, customer, amount, to_char(order_date, 'YYYY-MM-DD') from pythia_import_t order by order_id", &[])
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        [
            (1, Some("Acme".into()), 1234.5, "2024-12-31".into()),
            (2, None, 7.0, "2025-01-01".into()),
            (3, Some("Globex".into()), -0.5, "2024-06-15".into()),
        ]
    );
    conn.execute("drop table pythia_import_t purge", &[])
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

/// Debugs a block calling `demo_pkg.raise_salary` with a breakpoint in the
/// package body: stops at the line, shows variables, steps and finishes.
/// A debug run whose session can't connect to the debugger (no network ACL
/// entry names its host) ends instead of waiting for it.
#[test]
fn debug_run_that_cannot_connect() {
    let Some((profile, password)) = profile() else {
        eprintln!("PYTHIA_TEST_DB not set, skipping");
        return;
    };
    let mut h = headless_harness();
    h.state_mut().test_settings_mut().debug_host = "no-acl.invalid".into();
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin\n  null;\nend;".into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::DebugStart);
    wait_until(&mut h, "the run to end", |app| !app.test_debug_running());
    h.run_steps(2);
    let debug = h.state().test_debug();
    assert_eq!(
        debug.state,
        pythia_debugger::ui::State::Finished,
        "{:?}",
        debug.log
    );
    // The panel says why, and the setup opens for it.
    let problem = debug.problem.clone().expect("the problem");
    assert!(
        problem.starts_with("No network ACL entry lets the database connect to no-acl.invalid"),
        "{problem}"
    );
    let setup = h.state_mut().test_setup_window();
    assert!(setup.open);
    assert_eq!(setup.test_reason(), Some(problem.as_str()));
}

/// The debugger setup, end to end: a user without a network ACL entry
/// starts debugging; the setup opens by itself, finds that every host
/// fails on the ACL, offers the entry, runs it as the DBA, and then
/// debugging works.
#[test]
fn debugger_setup_fixes_the_acl() {
    let (Some((profile, _)), Ok(dba)) = (profile(), std::env::var("PYTHIA_TEST_DB_DBA")) else {
        eprintln!("PYTHIA_TEST_DB / PYTHIA_TEST_DB_DBA not set, skipping");
        return;
    };
    let _accounts = crate::test_db::ACCOUNTS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Entries outlive their user: drop those of an earlier run too.
    let cleanup = [
        "begin
           for a in (select host, lower_port, upper_port from dba_host_aces
                      where principal = 'PYTHIA_DBG_SETUP') loop
             dbms_network_acl_admin.remove_host_ace(host => a.host,
               lower_port => a.lower_port, upper_port => a.upper_port,
               ace => xs$ace_type(privilege_list => xs$name_list('jdwp'),
                                  principal_name => 'PYTHIA_DBG_SETUP',
                                  principal_type => xs_acl.ptype_db),
               remove_empty_acl => true);
           end loop;
         end;",
        "begin
           for s in (select sid, serial# from v$session
                      where username = 'PYTHIA_DBG_SETUP') loop
             execute immediate 'alter system kill session '''
               || s.sid || ',' || s.serial# || ''' immediate';
           end loop;
           execute immediate 'drop user pythia_dbg_setup cascade';
         exception when others then
           if sqlcode != -1918 then raise; end if;
         end;",
    ];
    crate::test_db::as_dba(&cleanup);
    crate::test_db::as_dba(&[
        "create user pythia_dbg_setup identified by pythia",
        "grant create session, debug connect session to pythia_dbg_setup",
    ]);
    let user = ConnectionProfile {
        name: "pythia_dbg_setup@FREEPDB1".into(),
        username: "pythia_dbg_setup".into(),
        ..profile
    };
    let mut h = harness(true).alone();
    h.state_mut().test_connect(user, "pythia".into());
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia_dbg_setup@FREEPDB1".into(),
        text: "begin\n  null;\nend;".into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::DebugStart);
    wait_until(&mut h, "the run to end", |app| !app.test_debug_running());
    wait_until(&mut h, "the setup's checks", |app| {
        let setup = app.test_setup_window_ref();
        setup.open && setup.test_done()
    });
    let setup = h.state_mut().test_setup_window();
    let hosts = setup.test_hosts();
    assert!(
        hosts.iter().all(|(_, probe)| matches!(
            probe,
            Some(pythia_debugger::setup::Probe::Failed {
                problem: pythia_debugger::setup::Problem::Acl,
                ..
            })
        )),
        "{hosts:?}"
    );
    // Only the entry: the user has the privilege. It's for the address the
    // database sees for this computer.
    let fix = setup.test_fix().expect("a fix");
    assert_eq!(fix.len(), 1, "{fix:?}");
    assert!(fix[0].contains("append_host_ace("), "{fix:?}");
    assert!(
        fix[0].contains("principal_name => 'PYTHIA_DBG_SETUP'"),
        "{fix:?}"
    );
    save(&mut h, "debug-setup-acl");

    let (credentials, _) = dba.rsplit_once('@').unwrap();
    let (dba_user, dba_password) = credentials.split_once('/').unwrap();
    h.state_mut()
        .test_setup_window()
        .test_run_as(dba_user, dba_password);
    h.run_steps(2);
    h.get_by_label("Run statements").scroll_to_me();
    // The scroll is animated: click where the button ends up.
    h.run_steps(30);
    h.get_by_label("Run statements").click();
    h.run_steps(2);
    let start = Instant::now();
    loop {
        let setup = h.state().test_setup_window_ref();
        if setup.test_done() && setup.test_works() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "after the fix: {:?} {:?}",
            setup.test_notice(),
            setup.test_hosts()
        );
        h.step();
        std::thread::sleep(Duration::from_millis(15));
    }
    save(&mut h, "debug-setup-works");

    // Debugging works now, with the host the entry is for.
    h.state_mut()
        .test_command(crate::commands::Command::DebugStart);
    wait_until(&mut h, "the second run to end", |app| {
        !app.test_debug_running()
    });
    let debug = h.state().test_debug();
    assert_eq!(debug.problem, None, "{:?}", debug.log);
    assert!(
        debug.log.iter().any(|l| l.starts_with("Connected")),
        "{:?}",
        debug.log
    );
    drop(h);
    crate::test_db::as_dba(&cleanup);
}

/// Needs DEBUG CONNECT SESSION / DEBUG ANY PROCEDURE and a JDWP ACL for the
/// test user (see pythia-debugger's setup script) and PYTHIA_DEBUG_HOST.
#[test]
fn debugger_session() {
    let (Some((profile, password)), Ok(host)) = (profile(), std::env::var("PYTHIA_DEBUG_HOST"))
    else {
        eprintln!("PYTHIA_TEST_DB / PYTHIA_DEBUG_HOST not set, skipping");
        return;
    };
    let setup = pythia_debugger::target::setup_statements(&profile.username, &host);
    crate::test_db::as_dba(&setup.iter().map(String::as_str).collect::<Vec<_>>());
    use pythia_debugger::{Unit, UnitType};
    let mut h = harness(true).alone();
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let unit = Unit::new("PYTHIA", "DEMO_PKG", UnitType::PackageBody);
    // Line 4 of the body: the UPDATE inside raise_salary. It only stops for
    // employee 1 (the condition skips the first call).
    h.state_mut().test_debug_setup(&host, unit.clone(), 4);
    let bp = pythia_debugger::Breakpoint {
        unit: unit.clone(),
        line: 4,
    };
    {
        let debug = h.state_mut().test_debug_mut();
        debug.set_options(
            &bp,
            pythia_debugger::BreakpointOptions {
                condition: Some("p_employee_id = 1".into()),
                hit_count: None,
            },
        );
        debug.add_watch("p_percent");
        debug.add_watch("p_employee_id * 2");
    }
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin\n  demo_pkg.raise_salary(p_employee_id => 2, p_percent => 0);\n  \
               demo_pkg.raise_salary(p_employee_id => 1, p_percent => 0);\n  rollback;\nend;"
            .into(),
        run: false,
    });
    h.run_steps(2);
    let sql = h.state().test_focused();
    h.state_mut()
        .test_command(crate::commands::Command::DebugStart);
    wait_until(&mut h, "breakpoint", |app| app.test_debug().is_suspended());
    let stack = &h.state().test_debug().stack;
    assert_eq!(stack[0].location.unit, unit, "{stack:?}");
    assert_eq!(stack[0].location.line, Some(4));
    let vars: Vec<String> = h
        .state()
        .test_debug()
        .variables
        .iter()
        .map(|v| format!("{}={}", v.name, v.value.as_deref().unwrap_or("NULL")))
        .collect();
    assert!(
        vars.iter()
            .any(|v| v.eq_ignore_ascii_case("p_employee_id=1")),
        "condition skipped employee 2: {vars:?}"
    );
    let debug = h.state().test_debug();
    assert_eq!(debug.hits.get(&bp), Some(&1));
    let watches: Vec<(String, Result<Option<String>, String>)> = debug
        .watch_values
        .iter()
        .map(|w| (w.expression.clone(), w.value.clone().map(|v| v.value)))
        .collect();
    assert_eq!(watches[0], ("p_percent".into(), Ok(Some("0".into()))));
    assert!(
        watches[1].1.is_err(),
        "expressions aren't evaluated: {watches:?}"
    );
    // Hovering a parameter in the stopped package body shows its value
    // (once the body's source has loaded).
    let needle = "where employee_id = p_employee_id";
    let start = Instant::now();
    let offset = loop {
        let program = h.state_mut().test_program();
        let editor = program.active_editor().expect("package body editor");
        if let Some(at) = editor.text().find(needle) {
            break at + "where employee_id = ".len() + 3;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "raise_salary body not shown: {:?}",
            editor.text()
        );
        h.step();
        std::thread::sleep(Duration::from_millis(15));
    };
    let debug = h.state().test_debug();
    assert_eq!(
        debug.hover(&unit, "P_EMPLOYEE_ID").map(|(_, v)| v),
        Some("1".into())
    );
    // The app borrows both; take the session out for the call.
    let mut session = pythia_debugger::ui::DebugSession::default();
    std::mem::swap(&mut session, h.state_mut().test_debug_mut());
    let program = h.state_mut().test_program();
    let editor = program.active_editor().unwrap();
    assert!(crate::editor::debug_hover(editor, offset, &session, &unit));
    std::mem::swap(&mut session, h.state_mut().test_debug_mut());
    h.run_steps(6);
    save(&mut h, "12-debugger-dark");

    // The panel says why a value can't be changed (raise_salary has only
    // IN parameters; changes themselves are pythia-debugger's tests).
    h.state_mut()
        .test_debug_mut()
        .set_value("p_percent", Some("5".into()));
    wait_until(&mut h, "refused change", |app| {
        app.test_debug().log.last().map(String::as_str)
            == Some("Can't set p_percent: it can't be changed (IN parameters are read-only)")
    });

    // Run to the block's `rollback`, from the cursor in the SQL window.
    h.state_mut().test_focus(sql);
    {
        let editor = &mut h.state_mut().test_sql_window().editor;
        let at = editor.text().find("rollback").unwrap();
        editor.select(at..at);
    }
    h.state_mut()
        .test_command(crate::commands::Command::DebugRunToCursor);
    wait_until(&mut h, "run to cursor", |app| {
        app.test_debug().is_suspended() || !app.test_debug_running()
    });
    let debug = h.state().test_debug();
    assert_eq!(
        debug.state,
        pythia_debugger::ui::State::Suspended {
            reason: pythia_debugger::StopReason::Cursor
        },
        "{:?}",
        debug.log
    );
    let frame = debug.current_frame().unwrap();
    assert_eq!(frame.location.unit.unit_type, UnitType::Block);
    assert_eq!(frame.location.line, Some(4), "the block's 4th line");
    h.state_mut()
        .test_debug_mut()
        .command(pythia_debugger::DebugCommand::Resume);
    wait_until(&mut h, "finish", |app| !app.test_debug_running());
}

/// Orders mostly shipped, a few new: without a histogram the optimizer
/// expects half of them new.
const SHOP: &[&str] = &[
    "create table shop_customers as
       select level id, 'Customer ' || level name from dual connect by level <= 500",
    "create table shop_orders as
       select level id, mod(level, 500) + 1 customer_id,
              case when level <= 200 then 'NEW' else 'SHIPPED' end status,
              rpad('x', 60, 'x') note
         from dual connect by level <= 50000",
    "begin
       dbms_stats.gather_table_stats(user, 'SHOP_CUSTOMERS');
       dbms_stats.gather_table_stats(user, 'SHOP_ORDERS', method_opt => 'for all columns size 1');
     end;",
    "create or replace procedure ship_new_orders is
  l_shipped pls_integer := 0;
begin
  for o in (select o.id, c.name
              from shop_orders o
              join shop_customers c on c.id = o.customer_id
             where o.status = 'NEW') loop
    l_shipped := l_shipped + 1;
  end loop;
  dbms_output.put_line(l_shipped || ' orders shipped');
end;",
];

/// The SQL panel while stopped: the loop's statement, matched to its line,
/// with the estimated and actual rows of its plan.
#[test]
fn debugger_sql_panel() {
    let (Some((profile, password)), Ok(host)) = (profile(), std::env::var("PYTHIA_DEBUG_HOST"))
    else {
        eprintln!("PYTHIA_TEST_DB / PYTHIA_DEBUG_HOST not set, skipping");
        return;
    };
    let setup = pythia_debugger::target::setup_statements(&profile.username, &host);
    crate::test_db::as_dba(&setup.iter().map(String::as_str).collect::<Vec<_>>());
    let conn = crate::test_db::connect("PYTHIA_TEST_DB").unwrap();
    for drop in [
        "drop table shop_orders purge",
        "drop table shop_customers purge",
    ] {
        let _ = conn.execute(drop, &[]);
    }
    for ddl in SHOP {
        conn.execute(ddl, &[])
            .unwrap_or_else(|e| panic!("{ddl}: {e}"));
    }
    use pythia_debugger::{Unit, UnitType};
    let unit = Unit::new("PYTHIA", "SHIP_NEW_ORDERS", UnitType::Procedure);
    let mut h = Ui {
        harness: new_harness(true, out_dir().is_some(), egui::vec2(1440.0, 1000.0)),
        lock: Some(UI_LOCK.alone()),
    };
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    // After the loop.
    h.state_mut().test_debug_setup(&host, unit.clone(), 10);
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin\n  ship_new_orders;\nend;".into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::DebugStart);
    wait_until(&mut h, "breakpoint", |app| app.test_debug().is_suspended());
    wait_until(&mut h, "the loop's plan", |app| {
        app.test_debug().sql_plan().is_some_and(|p| p.measured())
    });
    let debug = h.state().test_debug();
    let statement = &debug.sql_statements()[0];
    assert_eq!(
        (statement.unit.as_ref(), statement.line),
        (Some(&unit), 4),
        "{:?}",
        debug.sql_statements()
    );
    let plan = debug.sql_plan().unwrap();
    assert!(
        plan.text.contains("WHERE O.STATUS = 'NEW'"),
        "{}",
        plan.text
    );
    let worst = pythia_ui::plan::worst_misestimate(&plan.rows).expect("a misestimate");
    assert_eq!(worst.object, "PYTHIA.SHOP_ORDERS", "{worst:?}");
    assert_eq!(worst.actual_rows, Some(200.0));
    assert!(worst.misestimate().unwrap() >= 100.0, "{worst:?}");
    save(&mut h, "13-debugger-sql-plan-dark");
    h.state_mut()
        .test_debug_mut()
        .command(pythia_debugger::DebugCommand::Resume);
    wait_until(&mut h, "finish", |app| !app.test_debug_running());
    drop(h);
    let _ = conn.execute("drop procedure ship_new_orders", &[]);
    for drop in [
        "drop table shop_orders purge",
        "drop table shop_customers purge",
    ] {
        let _ = conn.execute(drop, &[]);
    }
}

/// pythia-debugger's debugging scenario: order processing across packages.
const DEBUG_SCENARIO: &str = include_str!("../../pythia-debugger/tests/scenario/orders.sql");

/// The environment of the debugger scenario tests: the test user's profile
/// and password, the debugger host and the DBA connect spec.
fn debug_environment() -> Option<(ConnectionProfile, String, String, String)> {
    let (Some((profile, password)), Ok(host), Ok(dba)) = (
        profile(),
        std::env::var("PYTHIA_DEBUG_HOST"),
        std::env::var("PYTHIA_TEST_DB_DBA"),
    ) else {
        eprintln!("PYTHIA_TEST_DB / PYTHIA_TEST_DB_DBA / PYTHIA_DEBUG_HOST not set, skipping");
        return None;
    };
    Some((profile, password, host, dba))
}

/// Lets the test user debug (with the database connecting back to `host`)
/// and installs [`DEBUG_SCENARIO`], compiled without debug information.
/// Returns a connection of the test user. The tests share the scenario:
/// create the harness first, [alone](Ui::alone).
fn install_debug_scenario(host: &str, dba: &str) -> pythia_db::oracle::Connection {
    let connect = |spec: &str| {
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        pythia_db::oracle::Connection::connect(user, pw, db).unwrap()
    };
    let dba = connect(dba);
    for stmt in pythia_debugger::target::setup_statements("pythia", host) {
        dba.execute(&stmt, &[]).unwrap();
    }
    let conn = connect(&std::env::var("PYTHIA_TEST_DB").unwrap());
    for drop in [
        "drop trigger dbgx_orders_bu",
        "drop package dbgx_orders_api",
        "drop package dbgx_pricing",
        "drop package dbgx_types",
        "drop type dbgx_money force",
        "drop type dbgx_codes force",
        "drop table dbgx_audit purge",
        "drop table dbgx_order_lines purge",
        "drop table dbgx_orders purge",
        "drop table dbgx_customers purge",
    ] {
        let _ = conn.execute(drop, &[]);
    }
    for stmt in pythia_plsql::split_statements(DEBUG_SCENARIO) {
        let sql = stmt.text(DEBUG_SCENARIO);
        conn.execute(sql, &[])
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }
    conn
}

/// Line of a `-- @name` marker in the scenario's statement that starts
/// with `head` (e.g. `create or replace trigger dbgx_orders_bu`).
fn scenario_marker(head: &str, name: &str) -> u32 {
    let source = pythia_plsql::split_statements(DEBUG_SCENARIO)
        .into_iter()
        .map(|s| s.text(DEBUG_SCENARIO))
        .find(|s| s.to_lowercase().starts_with(head))
        .unwrap();
    source
        .lines()
        .position(|l| l.contains(&format!("-- @{name}")))
        .unwrap() as u32
        + 1
}

/// The debugger scenario from pythia-debugger's tests: the editors follow
/// every stop, from a package body to a trigger and back, and into the SQL
/// window's block. Needs PYTHIA_TEST_DB, PYTHIA_TEST_DB_DBA and
/// PYTHIA_DEBUG_HOST.
#[test]
fn debugger_follows_execution() {
    use pythia_debugger::{DebugCommand, Unit, UnitType};
    let Some((profile, password, host, dba)) = debug_environment() else {
        return;
    };
    let mut h = harness(true).alone();
    // Install the scenario, with debug information everywhere.
    let conn = install_debug_scenario(&host, &dba);
    let unit = |name: &str, t| Unit::new("PYTHIA", name, t);
    let api = unit("DBGX_ORDERS_API", UnitType::PackageBody);
    let trigger = unit("DBGX_ORDERS_BU", UnitType::Trigger);
    for u in [
        &api,
        &trigger,
        &unit("DBGX_PRICING", UnitType::PackageBody),
        &unit("DBGX_MONEY", UnitType::TypeBody),
    ] {
        pythia_debugger::target::compile_debug(&conn, u).unwrap();
    }
    let load_done = scenario_marker(
        "create or replace package body dbgx_orders_api",
        "load_done",
    );
    let on_update = scenario_marker("create or replace trigger dbgx_orders_bu", "trigger");

    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut()
        .test_debug_setup(&host, api.clone(), load_done);
    h.state_mut()
        .test_debug_setup(&host, trigger.clone(), on_update);
    // The block isn't the first statement: its lines start further down.
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "select count(*) from dbgx_orders;\n\n\
               declare\n  l_count pls_integer;\nbegin\n  dbgx_orders_api.process_all(l_count);\n  \
               dbms_output.put_line('processed ' || l_count);\n  rollback;\nend;"
            .into(),
        run: false,
    });
    let sql_doc = h.state().test_focused();
    h.state_mut().test_sql_window().editor.goto(4, 3);
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::DebugStart);

    // Waits until the focused editor marks a line containing `needle`.
    let follow = |h: &mut Ui, needle: &str| -> (String, usize) {
        let start = Instant::now();
        loop {
            let (title, marker, cursor) = h.state().test_execution_marker();
            if let Some((line, text)) = &marker
                && text.contains(needle)
                && h.state().test_debug().is_suspended()
            {
                assert_eq!(cursor, *line, "the cursor is on the marked line in {title}");
                return (title, *line);
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "no marker at {needle}: {title} {marker:?}, log {:?}",
                h.state().test_debug().log
            );
            h.step();
            std::thread::sleep(Duration::from_millis(15));
        }
    };
    // Steps and waits for the next stop (commands while running are
    // ignored, as the toolbar's are disabled then).
    let step = |h: &mut Ui, command: DebugCommand| {
        let resume = command == DebugCommand::Resume;
        h.state_mut().test_debug_mut().command(command);
        if !resume {
            wait_until(h, "stop", |app| app.test_debug().is_suspended());
        }
    };

    let (title, line) = follow(&mut h, "-- @load_done");
    assert_eq!(
        (title.as_str(), line),
        ("dbgx_orders_api", load_done as usize)
    );
    step(&mut h, DebugCommand::StepOver);
    step(&mut h, DebugCommand::StepOver);
    // The marker moved within the window (the same breakpoint isn't left).
    follow(&mut h, "-- @process_loaded");
    step(&mut h, DebugCommand::Resume);
    let (title, line) = follow(&mut h, "-- @trigger");
    assert_eq!(title, "dbgx_orders_bu");
    assert_eq!(line, on_update as usize, "trigger lines match the editor");
    step(&mut h, DebugCommand::Resume);
    // Back to the package for the next order, then out to the block.
    let (title, _) = follow(&mut h, "-- @load_done");
    assert_eq!(title, "dbgx_orders_api");
    for bp in [(api.clone(), load_done), (trigger.clone(), on_update)] {
        h.state_mut().test_debug_mut().toggle_breakpoint(bp.0, bp.1);
    }
    assert!(h.state().test_debug().breakpoints.is_empty());
    for _ in 0..3 {
        step(&mut h, DebugCommand::StepOut);
    }
    let (title, line) = follow(&mut h, "dbms_output.put_line");
    assert_eq!(h.state().test_focused(), sql_doc, "{title}");
    assert_eq!(line, 7, "the block's line 5 in the SQL window");
    step(&mut h, DebugCommand::Resume);
    wait_until(&mut h, "finish", |app| !app.test_debug_running());
    h.run_steps(3);
    let (_, marker, _) = h.state().test_execution_marker();
    assert_eq!(marker, None, "no marker once finished");
}

/// "Compile with debug information" (Settings → Debugger): a package body
/// compiled in its program window gets debug information without new
/// messages, and Step into enters it. Selecting the caller in the call
/// stack shows its source, variables and hover values. Needs
/// PYTHIA_TEST_DB, PYTHIA_TEST_DB_DBA and PYTHIA_DEBUG_HOST.
#[test]
fn debugger_steps_into_unit_compiled_in_program_window() {
    use pythia_debugger::{DebugCommand, Unit, UnitType};
    let Some((profile, password, host, dba)) = debug_environment() else {
        return;
    };
    let mut h = harness(true).alone();
    let conn = install_debug_scenario(&host, &dba);
    let api = Unit::new("PYTHIA", "DBGX_ORDERS_API", UnitType::PackageBody);
    let pricing = Unit::new("PYTHIA", "DBGX_PRICING", UnitType::PackageBody);
    pythia_debugger::target::compile_debug(&conn, &api).unwrap();
    let process_line = scenario_marker(
        "create or replace package body dbgx_orders_api",
        "process_line",
    );

    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Program {
        connection: "pythia@FREEPDB1".into(),
        object: ObjectRef {
            owner: "PYTHIA".into(),
            name: "DBGX_PRICING".into(),
            object_type: pythia_db::ObjectType::PackageBody,
        },
    });
    wait_until(&mut h, "source", |app| !app.test_focused_busy());
    let has_debug_info = || pythia_debugger::target::has_debug_info(&conn, &pricing).unwrap();
    let compile = |h: &mut Ui, debug_info: bool| -> String {
        h.state_mut().test_settings_mut().compile_debug_info = debug_info;
        h.state_mut().test_execute_focused();
        wait_until(h, "compile", |app| !app.test_focused_busy());
        let program = h.state_mut().test_program();
        let (failed, message) = program.message().expect("compile message");
        assert!(!failed, "{message}");
        for part in &program.parts {
            assert!(part.errors.is_empty(), "{:?}", part.errors);
        }
        message
    };
    let message = compile(&mut h, false);
    assert!(!message.contains("debug"), "{message}");
    assert!(!has_debug_info());
    let message = compile(&mut h, true);
    assert!(message.ends_with("(with debug information)"), "{message}");
    assert!(has_debug_info());

    // Step into the call from the caller's breakpoint.
    h.state_mut()
        .test_debug_setup(&host, api.clone(), process_line);
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "declare\n  l_count pls_integer;\nbegin\n  dbgx_orders_api.process_all(l_count);\n  \
               rollback;\nend;"
            .into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::DebugStart);
    wait_until(&mut h, "breakpoint", |app| app.test_debug().is_suspended());
    assert_eq!(h.state().test_debug().stack[0].location.unit, api);
    h.state_mut()
        .test_debug_mut()
        .command(DebugCommand::StepInto);
    wait_until(&mut h, "step", |app| app.test_debug().is_suspended());
    let debug = h.state().test_debug();
    assert_eq!(debug.stack[0].location.unit, pricing, "{:?}", debug.stack);
    assert_eq!(debug.frame, 0);
    assert!(debug.variables.iter().all(|v| v.name != "L_TOTAL"));

    // The caller, from the call stack.
    h.get_by_label(&format!("dbgx_orders_api.process:{process_line}"))
        .click();
    wait_until(&mut h, "the caller's variables", |app| {
        let debug = app.test_debug();
        debug.frame == 1 && debug.variables.iter().any(|v| v.name == "L_TOTAL")
    });
    // DBGX_TYPES has no debug information: its record comes without
    // contents, and the variables view says why.
    wait_until(&mut h, "why l_order has no contents", |app| {
        app.test_debug().opaque_reason(&api, "L_ORDER").is_some()
    });
    assert_eq!(
        h.state().test_debug().opaque_reason(&api, "L_ORDER"),
        Some("T_ORDER is declared in DBGX_TYPES, compiled without debug information")
    );
    let (title, _, cursor) = h.state().test_execution_marker();
    assert_eq!(
        (title.as_str(), cursor),
        ("dbgx_orders_api", process_line as usize),
        "the caller's source"
    );
    let debug = h.state().test_debug();
    assert_eq!(
        debug.hover(&api, "l_total").map(|(_, v)| v),
        Some("0".into())
    );
    assert_eq!(debug.hover(&pricing, "l_total"), None);
    // Hovering it in the package body's editor.
    let mut session = pythia_debugger::ui::DebugSession::default();
    std::mem::swap(&mut session, h.state_mut().test_debug_mut());
    let editor = h
        .state_mut()
        .test_program_editor()
        .expect("package body editor");
    let offset = editor.text().find("l_total := l_total").unwrap() + 2;
    assert!(crate::editor::debug_hover(editor, offset, &session, &api));
    std::mem::swap(&mut session, h.state_mut().test_debug_mut());
    save(&mut h, "12b-debugger-caller-dark");

    h.state_mut().test_debug_mut().stop();
    wait_until(&mut h, "finish", |app| !app.test_debug_running());
}

/// Runs a deployment script that includes another (`@@`), continues after
/// an error (`WHENEVER SQLERROR CONTINUE`) and creates and fills a table.
#[test]
fn deployment_script() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("pythia-deploy-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(
        dir.join("install.sql"),
        "prompt Installing\nwhenever sqlerror continue\ndrop table pythia_deploy_t purge;\n@@sub/tables\ninsert into pythia_deploy_t values (2);\ncommit;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("sub/tables.sql"),
        "create table pythia_deploy_t (id number);\ninsert into pythia_deploy_t values (1);\n",
    )
    .unwrap();

    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    // The script expects the table not to exist (left over by a failed run).
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin
  for t in (select table_name from user_tables where table_name = 'PYTHIA_DEPLOY_T') loop
    execute immediate 'drop table pythia_deploy_t purge';
  end loop;
end;
/"
        .into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
    h.state_mut().test_open_file(dir.join("install.sql"));
    h.run_steps(2);
    save(&mut h, "deployment-script");
    h.state_mut()
        .test_command(crate::commands::Command::ExecuteAll);
    wait_until(&mut h, "script", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    let messages: Vec<String> = w.state.messages.iter().map(|m| m.text.clone()).collect();
    assert!(
        messages.iter().any(|m| m.contains("continuing"))
            || messages.iter().any(|m| m.contains("ORA-00942")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("Running ") && m.contains("tables.sql")),
        "{messages:?}"
    );
    assert_eq!(w.state.output, ["Installing"]);

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "select count(*) from pythia_deploy_t;\ndrop table pythia_deploy_t purge;".into(),
        run: true,
    });
    wait_until(&mut h, "check", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    assert_eq!(w.state.results[0].rows[0][0].as_deref(), Some("2"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Compiling a package body that someone else changed in the meantime asks
/// before overwriting.
#[test]
fn compile_conflict() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let body = ObjectRef {
        owner: "PYTHIA".into(),
        name: "DEMO_PKG".into(),
        object_type: pythia_db::ObjectType::PackageBody,
    };
    h.state_mut().test_open(OpenRequest::Program {
        connection: "pythia@FREEPDB1".into(),
        object: body.clone(),
    });
    wait_until(&mut h, "source", |app| !app.test_focused_busy());
    let program = h.state().test_focused();
    let source = h.state_mut().test_program().active_source();

    // Someone else recompiles the body a moment later.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let sql = source
        .trim_end()
        .trim_end_matches('/')
        .trim_end()
        .to_owned();
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: format!(
            "{}\n/\n",
            sql.replacen(
                "end demo_pkg;",
                "  -- changed by someone else\nend demo_pkg;",
                1
            )
        ),
        run: true,
    });
    wait_until(&mut h, "other compile", |app| !app.test_focused_busy());
    h.state_mut().test_close_focused();
    h.state_mut().test_focus(program);

    let effects = h.state_mut().test_program().compile(false, false);
    h.state_mut().test_apply_focused(effects);
    wait_until(&mut h, "check", |app| !app.test_focused_busy());
    assert!(h.state_mut().test_program().has_conflict());
    save(&mut h, "17-compile-conflict-dark");

    let effects = h.state_mut().test_program().overwrite();
    h.state_mut().test_apply_focused(effects);
    wait_until(&mut h, "compile", |app| !app.test_focused_busy());
    let message = h.state_mut().test_program().message();
    assert!(
        matches!(&message, Some((false, m)) if m.contains("compiled")),
        "{message:?}"
    );
}

/// A result shown as a bar chart and as a line chart.
#[test]
fn result_chart() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut()
        .test_connect(profile.clone(), password.clone());
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "-- Orders and returns per month
select to_char(add_months(date '2025-01-01', level - 1), 'Mon YYYY') as month,
       round(120 + 40 * sin(level / 2) + level * 6) as orders,
       round(50 + 20 * cos(level / 3) + level * 2) as returns
  from dual connect by level <= 12"
            .into(),
        run: true,
    });
    wait_until(&mut h, "query", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    let result = &mut w.state.results[0];
    let chart = crate::chart_view::ChartState::guess(&result.columns, result.rows.len());
    assert_eq!(chart.x, Some(0));
    assert_eq!(chart.series, vec![1, 2]);
    result.chart = Some(chart);
    save(&mut h, "result-chart-bars");
    let w = h.state_mut().test_sql_window();
    w.state.results[0].chart.as_mut().unwrap().kind = crate::chart_view::ChartKind::Line;
    save(&mut h, "result-chart-lines");
    let text = h.state_mut().test_sql_window().editor.text().to_owned();
    drop(h);

    // A narrow window: the twelve months' labels thin out evenly (#22).
    let mut h = sized_harness(egui::vec2(600.0, 700.0));
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text,
        run: true,
    });
    wait_until(&mut h, "query", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    let result = &mut w.state.results[0];
    result.chart = Some(crate::chart_view::ChartState::guess(
        &result.columns,
        result.rows.len(),
    ));
    save(&mut h, "result-chart-narrow");
}

/// A result of a table with foreign keys offers its parent and child rows.
#[test]
fn foreign_key_links() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin
  for t in (select table_name from user_tables where table_name in ('PYTHIA_FK_C', 'PYTHIA_FK_P')) loop
    execute immediate 'drop table ' || t.table_name || ' cascade constraints purge';
  end loop;
end;
/
create table pythia_fk_p (id number primary key, name varchar2(20));
create table pythia_fk_c (id number primary key, p_id number references pythia_fk_p);
insert into pythia_fk_p values (1, 'one');
insert into pythia_fk_c values (10, 1);
select * from pythia_fk_c;"
            .into(),
        run: true,
    });
    wait_until(&mut h, "script", |app| !app.test_focused_busy());
    let start = Instant::now();
    loop {
        let w = h.state_mut().test_sql_window();
        if !w.state.results[0].grid.links.is_empty() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "no links");
        h.step();
        std::thread::sleep(Duration::from_millis(15));
    }
    let w = h.state_mut().test_sql_window();
    let result = &w.state.results[0];
    assert_eq!(result.grid.links.len(), 1);
    assert!(
        result.grid.links[0]
            .label
            .starts_with("PYTHIA_FK_P via P_ID")
    );
    let relation = &result.relations[result.link_relations[0]];
    let sql = crate::relations::query(relation, &result.columns, &result.rows[0]);
    assert!(sql.contains("where ID = 1"), "{sql}");
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop table pythia_fk_c purge;\ndrop table pythia_fk_p purge;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// The scheduler jobs window lists a job, runs it and shows the run.
#[test]
fn scheduler_jobs() {
    let Some((profile, password)) = profile() else {
        return;
    };
    // Before 23ai, a new user can't create jobs.
    crate::test_db::as_dba(&["grant create job to {user}"]);
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin
  for j in (select job_name from user_scheduler_jobs
             where job_name in ('PYTHIA_NIGHTLY_STATS', 'PYTHIA_PURGE_LOGS')) loop
    dbms_scheduler.drop_job(j.job_name, force => true);
  end loop;
  dbms_scheduler.create_job('PYTHIA_NIGHTLY_STATS', 'PLSQL_BLOCK',
    'declare t timestamp := systimestamp + interval ''0.2'' second; begin while systimestamp < t loop null; end loop; end;',
    repeat_interval => 'FREQ=DAILY;BYHOUR=2', enabled => true,
    comments => 'Refreshes optimizer statistics of the schema every night');
  dbms_scheduler.create_job('PYTHIA_PURGE_LOGS', 'PLSQL_BLOCK', 'begin null; end;',
    repeat_interval => 'FREQ=HOURLY', enabled => false);
end;
/"
        .into(),
        run: true,
    });
    wait_until(&mut h, "jobs created", |app| !app.test_focused_busy());
    h.state_mut()
        .test_command(crate::commands::Command::SchedulerJobs);
    h.run_steps(2);
    wait_until(&mut h, "jobs", |app| !app.test_focused_busy());
    let jobs = h.state_mut().test_jobs_window();
    assert!(
        jobs.jobs()
            .iter()
            .any(|j| j.name == "PYTHIA_NIGHTLY_STATS" && j.enabled)
    );
    assert!(
        jobs.jobs()
            .iter()
            .any(|j| j.name == "PYTHIA_PURGE_LOGS" && !j.enabled)
    );
    let effects = jobs.select("PYTHIA", "PYTHIA_NIGHTLY_STATS");
    h.state_mut().test_apply(effects);
    let effects = h.state_mut().test_jobs_window().run_action("run");
    h.state_mut().test_apply(effects);
    let start = Instant::now();
    while h.state_mut().test_jobs_window().runs().is_empty() {
        assert!(start.elapsed() < Duration::from_secs(60), "no run logged");
        std::thread::sleep(Duration::from_millis(500));
        let effects = h.state_mut().test_jobs_window().refresh();
        h.state_mut().test_apply(effects);
        wait_until(&mut h, "refresh", |app| !app.test_focused_busy());
    }
    assert_eq!(
        h.state_mut().test_jobs_window().runs()[0].status,
        "SUCCEEDED"
    );
    save(&mut h, "scheduler-jobs");

    // Edit… opens the job form; applying it refreshes the window.
    let jobs_window = h.state().test_focused();
    h.get_by_label("Edit…").click();
    h.run_steps(2);
    wait_until(&mut h, "job form", |app| !app.test_focused_busy());
    h.run_steps(2);
    wait_until(&mut h, "next runs", |app| !app.test_focused_busy());
    h.run_steps(2);
    assert_eq!(
        h.state().test_focused_title().as_deref(),
        Some("pythia_nightly_stats (job)")
    );
    assert!(h.query_by_label_contains("Next runs:").is_some());
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d.downcast_mut::<crate::object_editor::JobDef>().unwrap();
        d.repeat = "FREQ=DAILY;BYHOUR=3;BYMINUTE=15;BYSECOND=0".into();
    });
    h.run_steps(2);
    wait_until(&mut h, "next runs", |app| !app.test_focused_busy());
    h.run_steps(2);
    save(&mut h, "edit-job");
    assert_eq!(
        h.state_mut()
            .test_object_editor()
            .statements_for_test()
            .unwrap(),
        [
            "begin\n  dbms_scheduler.set_attribute('PYTHIA.PYTHIA_NIGHTLY_STATS', \
          'repeat_interval', 'FREQ=DAILY;BYHOUR=3;BYMINUTE=15;BYSECOND=0');\nend;"
        ]
    );
    let effects = h.state_mut().test_object_editor().apply_for_test();
    h.state_mut().test_apply(effects);
    wait_until(&mut h, "applied", |app| !app.test_focused_busy());
    h.state_mut().test_focus(jobs_window);
    h.run_steps(2);
    wait_until(&mut h, "jobs refreshed", |app| !app.test_focused_busy());
    let schedule = h
        .state_mut()
        .test_jobs_window()
        .jobs()
        .iter()
        .find(|j| j.name == "PYTHIA_NIGHTLY_STATS")
        .map(|j| j.schedule.clone());
    assert_eq!(
        schedule.as_deref(),
        Some("FREQ=DAILY;BYHOUR=3;BYMINUTE=15;BYSECOND=0")
    );
    // New job… opens an empty form for the browser's schema.
    h.get_by_label("New job…").click();
    h.run_steps(2);
    wait_until(&mut h, "new job form", |app| !app.test_focused_busy());
    assert_eq!(h.state().test_focused_title().as_deref(), Some("New job"));
    assert_eq!(
        h.state_mut().test_object_editor().statements_for_test(),
        Err("Enter a name".into())
    );

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin
  dbms_scheduler.drop_job('PYTHIA_NIGHTLY_STATS', force => true);
  dbms_scheduler.drop_job('PYTHIA_PURGE_LOGS', force => true);
end;
/"
        .into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// "Execute to file" streams a query into a file and can be cancelled.
#[test]
fn execute_to_file() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: String::new(),
        run: false,
    });
    h.run_steps(2);
    let dir = std::env::temp_dir().join(format!("pythia-to-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let start_export = |h: &mut Harness<'_, PythiaApp>, sql: &str, path: &std::path::Path| {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(path.to_owned())).unwrap();
        h.state_mut().test_sql_window().state.to_file_pick = Some((rx, sql.to_owned()));
        h.run_steps(2);
    };

    let path = dir.join("rows.json");
    start_export(
        &mut h,
        "select level as id, 'item ' || level as name from dual connect by level <= 5000",
        &path,
    );
    wait_until(&mut h, "export", |app| !app.test_focused_busy());
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 5000);
    assert_eq!(json[4999]["ID"], 5000);
    let w = h.state_mut().test_sql_window();
    assert!(
        w.state
            .messages
            .last()
            .unwrap()
            .text
            .starts_with("5000 rows written to"),
        "{:?}",
        w.state.messages.last().map(|m| &m.text)
    );

    // Cancel a long export: the incomplete file is removed.
    let path = dir.join("huge.csv");
    start_export(
        &mut h,
        "select a.n, b.n as m from (select level n from dual connect by level <= 100000) a,
                (select level n from dual connect by level <= 100000) b",
        &path,
    );
    wait_until(&mut h, "rows", |app| {
        app.test_sql_window_ref()
            .and_then(|w| w.state.to_file.as_ref())
            .is_some_and(|t| t.rows.load(std::sync::atomic::Ordering::Relaxed) > 0)
    });
    save(&mut h, "execute-to-file");
    h.state_mut().test_command(crate::commands::Command::Cancel);
    wait_until(&mut h, "cancel", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    let last = &w.state.messages.last().unwrap().text;
    assert!(last.contains("ORA-01013"), "{last}");
    assert!(!path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Tables in the browser show their indexes, constraints and triggers.
#[test]
fn browser_table_parts() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin
  for t in (select table_name from user_tables where table_name in ('ORDER_LINES', 'ORDERS')) loop
    execute immediate 'drop table ' || t.table_name || ' cascade constraints purge';
  end loop;
end;
/
create table orders (order_id number primary key, customer varchar2(40) not null,
  ordered_at date default sysdate not null);
create table order_lines (
  order_id number not null constraint order_lines_order_fk references orders,
  line_no number(4) not null,
  product varchar2(40) not null,
  quantity number(6) not null constraint order_lines_qty_ck check (quantity > 0),
  price number(10,2),
  constraint order_lines_pk primary key (order_id, line_no));
create index order_lines_product_ix on order_lines (product);
create or replace trigger order_lines_audit before insert or update on order_lines
for each row begin :new.price := round(:new.price, 2); end;
/"
        .into(),
        run: true,
    });
    wait_until(&mut h, "tables", |app| !app.test_focused_busy());
    h.state_mut()
        .test_expand_browser(&[pythia_db::ObjectType::Table]);
    wait_until(&mut h, "folder", |app| !app.test_browser_loading());
    h.state_mut().test_expand_table(ObjectRef {
        owner: "PYTHIA".into(),
        name: "ORDER_LINES".into(),
        object_type: pythia_db::ObjectType::Table,
    });
    wait_until(&mut h, "parts", |app| !app.test_browser_loading());
    save(&mut h, "browser-table-parts");
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop table order_lines purge;\ndrop table orders purge;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// A project folder under git: schema export, status, diff with HEAD and
/// with the database, deployment script.
#[test]
fn project_folder() {
    let Some((profile, password)) = profile() else {
        return;
    };
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "create or replace package pythia_proj_pkg as
  function answer return number;
end pythia_proj_pkg;
/
create or replace package body pythia_proj_pkg as
  function answer return number is
  begin
    return 42;
  end answer;
end pythia_proj_pkg;
/"
        .into(),
        run: true,
    });
    wait_until(&mut h, "package", |app| !app.test_focused_busy());

    let dir = std::env::temp_dir().join(format!("pythia-project-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The panel picks up results while it's visible.
    h.state_mut()
        .test_command(crate::commands::Command::ShowProject);
    let mut actions = Vec::new();
    h.state_mut()
        .test_project()
        .export_schema(dir.clone(), "pythia@FREEPDB1", &mut actions);
    h.state_mut().test_project_actions(actions);
    let start = Instant::now();
    while h.state_mut().test_project().is_busy() {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "export {:?}",
            h.state_mut().test_project().export_progress()
        );
        h.step();
        std::thread::sleep(Duration::from_millis(50));
    }
    let message = h
        .state_mut()
        .test_project()
        .message()
        .unwrap_or_default()
        .to_owned();
    assert!(message.starts_with("Exported "), "{message}");
    let body = dir.join("packages/pythia_proj_pkg.pkb");
    assert!(body.exists());

    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "T"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "export"]);
    let text = std::fs::read_to_string(&body).unwrap();
    std::fs::write(&body, text.replace("return 42;", "return 43;")).unwrap();

    let project = h.state_mut().test_project();
    project.open_root(dir.clone());
    project.refresh_now();
    project.expand(dir.join("packages"));
    let repo = project.repo().unwrap();
    let modified = repo.root.join("packages/pythia_proj_pkg.pkb");
    assert_eq!(repo.status[&modified], crate::git::FileStatus::Modified);
    h.state_mut()
        .test_command(crate::commands::Command::ShowProject);
    h.run_steps(3);
    save(&mut h, "project-panel");

    // Working copy against the database: the changed line differs.
    let mut actions = Vec::new();
    h.state_mut()
        .test_project()
        .compare_db(&modified, "pythia@FREEPDB1", &mut actions);
    h.state_mut().test_project_actions(actions);
    let start = Instant::now();
    while h.state_mut().test_project().is_busy() {
        assert!(start.elapsed() < Duration::from_secs(30), "compare");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    h.run_steps(3);
    let title = h.state().test_focused_title().unwrap_or_default();
    assert!(title.starts_with("pythia_proj_pkg.pkb ↔"), "{title}");
    save(&mut h, "project-compare-db");

    // And against the last commit.
    let mut actions = Vec::new();
    h.state_mut()
        .test_project()
        .diff_head(&modified, &mut actions);
    assert_eq!(actions.len(), 1);
    h.state_mut().test_project_actions(actions);
    h.run_steps(3);
    assert_eq!(
        h.state().test_focused_title().as_deref(),
        Some("pythia_proj_pkg.pkb (changes)")
    );

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop package pythia_proj_pkg;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The keyboard settings list commands with their shortcuts.
#[test]
fn settings_keyboard() {
    if out_dir().is_none() {
        return;
    }
    let mut h = harness(true);
    h.state_mut().test_settings_keyboard();
    h.run_steps(3);
    save(&mut h, "settings-keyboard");
}

/// A font file for code replaces Hack; a bad file is reported, not loaded.
#[test]
fn code_font() {
    let font = "/usr/share/fonts/truetype/noto/NotoMono-Regular.ttf";
    if !std::path::Path::new(font).exists() {
        return;
    }
    let mut h = harness(false);
    let ctx = h.ctx.clone();
    h.state_mut().test_code_font(&ctx, font);
    h.run_steps(3);
    save(&mut h, "code-font");
    let bad = not_a_font("code-font");
    h.state_mut().test_code_font(&ctx, &bad);
    h.run_steps(2);
    assert!(
        h.state()
            .test_log()
            .iter()
            .any(|l| l.contains(&format!("{bad} is not a TrueType or OpenType font")))
    );
    let _ = std::fs::remove_file(&bad);
}

/// Settings → Appearance applies right away: Compact tightens the layout,
/// a changed value makes it Custom, and an interface font replaces the
/// built-in one (a bad file is reported, not loaded).
#[test]
fn appearance_settings() {
    use pythia_config::Density;
    let mut h = harness(true);
    h.state_mut().test_settings_page(0);
    h.run_steps(3);
    let margin = |h: &Harness<'_, PythiaApp>| h.ctx.global_style().spacing.window_margin.left;
    assert_eq!(margin(&h), 14);
    assert!(h.query_by_label("Custom").is_none());
    h.get_by_label("Compact").click();
    h.run_steps(3);
    assert_eq!(
        h.state_mut().test_settings_mut().appearance.density,
        Density::COMPACT
    );
    assert_eq!(margin(&h), 6);
    h.state_mut().test_settings_mut().appearance.density.indent = 20.0;
    h.state_mut().test_settings_page(0);
    h.run_steps(3);
    assert!(h.query_by_label("Custom").is_some());

    let font = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
    if std::path::Path::new(font).exists() {
        h.state_mut().test_settings_mut().appearance.font_file = font.into();
        h.state_mut().test_settings_mut().appearance.font_size = 12.0;
        let ctx = h.ctx.clone();
        h.state_mut().test_apply_settings(&ctx);
        h.run_steps(3);
        let first = h
            .ctx
            .fonts(|f| f.definitions().families[&egui::FontFamily::Proportional][0].clone());
        assert_eq!(first, "ui");
        assert_eq!(
            h.ctx.global_style().text_styles[&egui::TextStyle::Body].size,
            12.0
        );
        save(&mut h, "settings-appearance");
    }
    let bad = not_a_font("appearance");
    h.state_mut().test_settings_mut().appearance.font_file = bad.clone();
    let ctx = h.ctx.clone();
    h.state_mut().test_apply_settings(&ctx);
    h.run_steps(2);
    let log = h.state().test_log();
    assert!(
        log.iter()
            .any(|l| l.contains(&format!("{bad} is not a TrueType or OpenType font"))),
        "{log:?}"
    );
    let _ = std::fs::remove_file(&bad);
    let first = h
        .ctx
        .fonts(|f| f.definitions().families[&egui::FontFamily::Proportional][0].clone());
    assert_ne!(first, "ui");
}

/// The SQL window, object browser and result grid as they come, and with
/// the Compact density, a smaller interface font, object names in it and
/// lines between rows.
#[test]
fn compact_layout() {
    if out_dir().is_none() {
        return;
    }
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: QUERY.into(),
        run: true,
    });
    wait_until(&mut h, "query", |app| !app.test_focused_busy());
    h.state_mut()
        .test_expand_browser(&[pythia_db::ObjectType::Table, pythia_db::ObjectType::Package]);
    wait_until(&mut h, "browser", |app| !app.test_browser_loading());
    save(&mut h, "comfortable-layout");
    let settings = h.state_mut().test_settings_mut();
    settings.appearance.density = pythia_config::Density::COMPACT;
    settings.appearance.font_size = 12.0;
    settings.appearance.names_in_code_font = false;
    settings.grid.font_size = 12.0;
    settings.grid.row_lines = true;
    settings.grid.null_text = "(null)".into();
    let ctx = h.ctx.clone();
    h.state_mut().test_apply_settings(&ctx);
    save(&mut h, "compact-layout");
}

/// While a menu of the bar is open, moving onto another title opens that
/// menu; without an open menu, hovering a title does nothing.
#[test]
fn menu_bar_switches_on_hover() {
    let mut h = headless_harness();
    h.run_steps(2);
    h.get_by_label("Edit").hover();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Format Code").is_none());
    h.get_by_label("File").click();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Save As").is_some());
    h.get_by_label("Edit").hover();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Format Code").is_some());
    assert!(h.query_by_label_contains("Save As").is_none());
}

/// Profiling a call shows where the time went, per subprogram.
#[test]
fn profiler() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "create or replace package pythia_prof_pkg as
  function slow_sum(p_n number) return number;
  procedure report;
end pythia_prof_pkg;
/
create or replace package body pythia_prof_pkg as
  function slow_sum(p_n number) return number is
    l_total number := 0;
  begin
    for i in 1 .. p_n loop
      l_total := l_total + sqrt(i);
    end loop;
    return l_total;
  end slow_sum;

  procedure report is
    l_count number;
  begin
    select count(*) into l_count from all_objects where rownum <= 2000;
    dbms_output.put_line(slow_sum(200000) + l_count);
  end report;
end pythia_prof_pkg;
/"
        .into(),
        run: true,
    });
    wait_until(&mut h, "package", |app| !app.test_focused_busy());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "exec pythia_prof_pkg.report".into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::Profile);
    wait_until(&mut h, "profile", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    let Some((_, Some(result))) = &w.state.profile else {
        panic!("no profile");
    };
    match result {
        Ok(p) => {
            let names: Vec<String> = p.rows.iter().map(|r| r.title()).collect();
            assert!(
                names.iter().any(|n| n == "PYTHIA.PYTHIA_PROF_PKG.SLOW_SUM"),
                "{names:?}"
            );
            assert!(p.rows.iter().any(|r| r.object().is_some()));
        }
        // No EXECUTE on DBMS_HPROF here, or a database before 18c.
        Err(e) if e.contains("grant execute") || e.contains("needs Oracle 18c") => return,
        Err(e) => panic!("{e}"),
    }
    save(&mut h, "profiler");
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop package pythia_prof_pkg;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// "Save as report" takes the query at the cursor.
#[test]
fn save_as_report() {
    if out_dir().is_none() {
        return;
    }
    let mut h = harness(false);
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "none".into(),
        text: "select trunc(ordered_at) as day, count(*) as orders\n  from orders\n where customer = :customer\n group by trunc(ordered_at)".into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::SaveAsReport);
    h.run_steps(3);
    save(&mut h, "save-as-report");
}

/// Export tables: pick tables, filters and format, export into a folder.
#[test]
fn export_tables() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut()
        .test_command(crate::commands::Command::ExportTables);
    h.run_steps(3);
    wait_until(&mut h, "tables", |app| !app.test_focused_busy());
    let dir = std::env::temp_dir().join(format!("pythia-export-tables-{}", std::process::id()));
    let export = h.state_mut().test_export_tables();
    export.folder = Some(dir.clone());
    export.include_ddl = true;
    export.set_tables(&[
        ("DEMO_DEPARTMENTS", ""),
        ("DEMO_EMPLOYEES", "department_id = 20"),
    ]);
    h.run_steps(2);
    save(&mut h, "export-tables");
    let effects = h.state_mut().test_export_tables().start_now();
    h.state_mut().test_apply(effects);
    wait_until(&mut h, "export", |app| !app.test_focused_busy());
    let message = h.state_mut().test_export_tables().message().cloned();
    assert!(
        matches!(&message, Some((false, m)) if m.starts_with("Exported ")),
        "{message:?}"
    );
    assert!(dir.join("demo_employees.csv").exists());
    assert!(dir.join("demo_departments.ddl.sql").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Views open on their DDL, loaded on the metadata session; the tab chosen
/// last is where the next view opens. The edit form reads WITH READ ONLY
/// from the dictionary.
#[test]
fn object_view_opens_on_the_ddl() {
    use crate::object_editor::EditTarget;
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "create or replace view pythia_ov_a as select 1 x, 'a' y from dual with read only;
create or replace view pythia_ov_b as select 2 z from dual;"
            .into(),
        run: true,
    });
    wait_until(&mut h, "views", |app| !app.test_focused_busy());
    let view = |name: &str| ObjectRef {
        owner: "PYTHIA".into(),
        name: name.into(),
        object_type: pythia_db::ObjectType::View,
    };
    let open = |h: &mut Harness<'_, PythiaApp>, name: &str| {
        h.state_mut().test_open(OpenRequest::Object {
            connection: "pythia@FREEPDB1".into(),
            object: view(name),
        });
        h.run_steps(2);
        wait_until(h, "object view", |app| !app.test_focused_busy());
    };

    open(&mut h, "PYTHIA_OV_A");
    assert!(h.state().test_meta_session("pythia@FREEPDB1"));
    let v = h.state().test_object_view();
    assert_eq!(v.tab_for_test(), Some("DDL"));
    let (ddl, columns) = v.loaded_for_test();
    assert!(ddl.is_some_and(|d| d.contains("PYTHIA_OV_A")));
    assert_eq!(columns, 0, "only the shown tab loads");

    h.get_by_label("Columns").hover();
    h.run_steps(1);
    h.get_by_label("Columns").click();
    h.run_steps(2);
    wait_until(&mut h, "columns", |app| !app.test_focused_busy());
    assert_eq!(h.state().test_object_view().loaded_for_test().1, 2);

    // The next view opens where the last one was left.
    open(&mut h, "PYTHIA_OV_B");
    let v = h.state().test_object_view();
    assert_eq!(v.tab_for_test(), Some("Columns"));
    assert_eq!(v.loaded_for_test(), (None, 1));

    h.state_mut().test_open(OpenRequest::EditObject {
        connection: "pythia@FREEPDB1".into(),
        target: EditTarget::Edit(view("PYTHIA_OV_A")),
    });
    h.run_steps(2);
    wait_until(&mut h, "editor", |app| !app.test_focused_busy());
    let mut loaded = None;
    h.state_mut().test_object_editor().edit_for_test(|d| {
        loaded = d.downcast_ref::<crate::object_editor::ViewDef>().cloned();
    });
    let d = loaded.expect("the view form");
    assert!(d.read_only && !d.check_option, "{d:?}");
    assert_eq!(d.query, "select 1 x, 'a' y from dual");

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop view pythia_ov_a;\ndrop view pythia_ov_b;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// The object editors: a new sequence applied through the form, then each
/// editor on existing objects.
#[test]
fn object_editors() {
    use crate::object_editor::EditTarget;
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    // PYTHIA_OEU_, not PYTHIA_OE_: the unit test `edits_live_objects`
    // drops those, and runs at the same time.
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin
  for o in (select object_name, object_type from user_objects
             where object_name like 'PYTHIA\\_OEU\\_%' escape '\\'
               and object_type in ('TABLE', 'VIEW', 'SEQUENCE', 'SYNONYM')) loop
    execute immediate 'drop ' || o.object_type || ' ' || o.object_name
      || case o.object_type when 'TABLE' then ' purge' end;
  end loop;
end;
/
create table pythia_oeu_orders (order_id number primary key, customer varchar2(40) not null,
  status varchar2(10), ordered_at date default sysdate not null);
create index pythia_oeu_orders_ix on pythia_oeu_orders (customer, ordered_at desc);
create or replace view pythia_oeu_open_orders as
  select order_id, customer, ordered_at from pythia_oeu_orders where status = 'OPEN'
  with check option;
create synonym pythia_oeu_ord for pythia_oeu_orders;
grant select, insert on pythia_oeu_orders to public;"
            .into(),
        run: true,
    });
    wait_until(&mut h, "objects", |app| !app.test_focused_busy());
    let object = |name: &str, t| ObjectRef {
        owner: "PYTHIA".into(),
        name: name.into(),
        object_type: t,
    };
    let open = |h: &mut Harness<'_, PythiaApp>, target: EditTarget| {
        h.state_mut().test_open(OpenRequest::EditObject {
            connection: "pythia@FREEPDB1".into(),
            target,
        });
        h.run_steps(2);
        wait_until(h, "editor", |app| !app.test_focused_busy());
    };

    // A new sequence, created through the form.
    open(
        &mut h,
        EditTarget::New {
            owner: "PYTHIA".into(),
            object_type: pythia_db::ObjectType::Sequence,
            from: None,
        },
    );
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d
            .downcast_mut::<crate::object_editor::SequenceDef>()
            .unwrap();
        d.name = "pythia_oeu_invoice_seq".into();
        d.start = "1000".into();
        d.cache = 50;
    });
    h.run_steps(2);
    save(&mut h, "editor-sequence-new");
    let effects = h.state_mut().test_object_editor().apply_for_test();
    h.state_mut().test_apply_focused(effects);
    wait_until(&mut h, "apply", |app| !app.test_focused_busy());
    let editor = h.state_mut().test_object_editor();
    assert_eq!(editor.outcome(), Some(("Applied", true)));
    assert_eq!(editor.title(), "pythia_oeu_invoice_seq (sequence)");
    // Reloaded from the database: nothing left to apply.
    assert_eq!(editor.statements_for_test(), Ok(Vec::new()));
    save(&mut h, "editor-sequence");

    open(
        &mut h,
        EditTarget::Edit(object(
            "PYTHIA_OEU_OPEN_ORDERS",
            pythia_db::ObjectType::View,
        )),
    );
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d.downcast_mut::<crate::object_editor::ViewDef>().unwrap();
        d.query = "select order_id, customer, ordered_at, status\n  from pythia_oeu_orders\n where status in ('OPEN', 'HOLD')".into();
    });
    h.run_steps(3);
    save(&mut h, "editor-view");
    let s = h
        .state_mut()
        .test_object_editor()
        .statements_for_test()
        .unwrap();
    assert!(s[0].ends_with("with check option"), "{s:?}");

    open(
        &mut h,
        EditTarget::Edit(object("PYTHIA_OEU_ORD", pythia_db::ObjectType::Synonym)),
    );
    save(&mut h, "editor-synonym");

    open(
        &mut h,
        EditTarget::Edit(object("PYTHIA_OEU_ORDERS_IX", pythia_db::ObjectType::Index)),
    );
    h.run_steps(3);
    wait_until(&mut h, "columns", |app| !app.test_focused_busy());
    save(&mut h, "editor-index");

    open(
        &mut h,
        EditTarget::Grants(object("PYTHIA_OEU_ORDERS", pythia_db::ObjectType::Table)),
    );
    save(&mut h, "editor-grants");
    assert!(
        h.state_mut()
            .test_object_editor()
            .statements_for_test()
            .unwrap()
            .is_empty()
    );

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop synonym pythia_oeu_ord;
drop view pythia_oeu_open_orders;
drop sequence pythia_oeu_invoice_seq;
drop table pythia_oeu_orders purge;"
            .into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// The table builder: a new table from the form, applied in the window,
/// becomes an edit of the created table, and the browser shows it.
#[test]
fn table_builder() {
    use pythia_compare::table::{ColumnDef, ConstraintDef, ConstraintKind, IndexDef};
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let cleanup = "begin
  for t in (select table_name from user_tables
             where table_name like 'PYTHIA\\_TBU\\_%' escape '\\'
             order by table_name desc) loop
    execute immediate 'drop table ' || t.table_name || ' cascade constraints purge';
  end loop;
end;
/
";
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: format!(
            "{cleanup}create table pythia_tbu_customers (id number(10) primary key, name varchar2(100));"
        ),
        run: true,
    });
    wait_until(&mut h, "setup", |app| !app.test_focused_busy());

    h.state_mut().test_open(OpenRequest::NewTable {
        connection: "pythia@FREEPDB1".into(),
        owner: "PYTHIA".into(),
        like: None,
    });
    h.run_steps(2);
    wait_until(&mut h, "builder", |app| !app.test_focused_busy());
    assert_eq!(h.state().test_focused_title().as_deref(), Some("New table"));
    let column = |name: &str, data_type: &str, nullable: bool| ColumnDef {
        name: name.into(),
        data_type: data_type.into(),
        nullable,
        default: None,
        comment: None,
        identity: false,
    };
    h.state_mut().test_table_editor().edit_for_test(|t| {
        t.name = "pythia_tbu_orders".into();
        t.columns[0].name = "order_id".into();
        t.constraints[0].columns = vec!["order_id".into()];
        t.columns[0].identity = true;
        t.columns.push(column("customer_id", "NUMBER(10)", false));
        t.columns.push(ColumnDef {
            default: Some("'OPEN'".into()),
            comment: Some("OPEN or DONE".into()),
            ..column("status", "VARCHAR2(10 CHAR)", false)
        });
        t.columns.push(column("total", "NUMBER(12,2)", true));
        t.comment = Some("Customer orders".into());
        t.constraints.push(ConstraintDef {
            name: String::new(),
            kind: ConstraintKind::Foreign,
            columns: vec!["customer_id".into()],
            references: Some(("pythia_tbu_customers".into(), vec!["id".into()])),
            on_delete: Some("CASCADE".into()),
            condition: None,
            generated: false,
        });
        t.constraints.push(ConstraintDef {
            name: String::new(),
            kind: ConstraintKind::Check,
            columns: Vec::new(),
            references: None,
            on_delete: None,
            condition: Some("status in ('OPEN', 'DONE')".into()),
            generated: false,
        });
        t.indexes.push(IndexDef {
            name: String::new(),
            unique: false,
            columns: vec!["customer_id".into()],
            generated: false,
        });
    });
    h.run_steps(3);
    save(&mut h, "table-builder-columns");
    h.get_by_label("Keys & checks").click();
    h.run_steps(3);
    save(&mut h, "table-builder-keys");
    h.get_by_label("Script •").click();
    h.run_steps(3);
    save(&mut h, "table-builder-script");

    let effects = h.state_mut().test_table_editor().apply_for_test();
    h.state_mut().test_apply_focused(effects);
    h.run_steps(2);
    wait_until(&mut h, "apply", |app| !app.test_focused_busy());
    let editor = h.state_mut().test_table_editor();
    assert_eq!(editor.outcome(), Some(("Applied", true)));
    assert_eq!(editor.title(), "pythia_tbu_orders (edit)");
    assert_eq!(editor.statements_for_test(), Ok(Vec::new()));
    // The browser lists and selects the new table.
    assert_eq!(
        h.state().test_browser_selected(),
        Some(ObjectRef {
            owner: "PYTHIA".into(),
            name: "PYTHIA_TBU_ORDERS".into(),
            object_type: pythia_db::ObjectType::Table,
        })
    );
    wait_until(&mut h, "browser", |app| !app.test_browser_loading());
    save(&mut h, "table-builder-created");

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: cleanup.into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// The table builder's Partitions tab: a range-partitioned table with a
/// MAXVALUE partition, created and shown as an existing table's partitions.
#[test]
fn table_builder_partitions() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let drop = "begin execute immediate 'drop table pythia_tbp_sales purge';
  exception when others then null; end;";
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: drop.into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
    h.state_mut().test_open(OpenRequest::NewTable {
        connection: "pythia@FREEPDB1".into(),
        owner: "PYTHIA".into(),
        like: None,
    });
    h.run_steps(2);
    wait_until(&mut h, "builder", |app| !app.test_focused_busy());
    h.state_mut().test_table_editor().edit_for_test(|t| {
        t.name = "pythia_tbp_sales".into();
        t.columns.push(pythia_compare::table::ColumnDef {
            name: "SOLD".into(),
            data_type: "DATE".into(),
            nullable: false,
            default: None,
            comment: None,
            identity: false,
        });
    });
    h.get_by_label("Partitions").hover();
    h.run_steps(1);
    h.get_by_label("Partitions").click();
    h.run_steps(2);
    // A combo box shows its choice as its value.
    h.get_by_value("not partitioned").click();
    h.run_steps(2);
    h.get_by_label("range").click();
    h.run_steps(2);
    h.state_mut()
        .test_table_editor()
        .set_partition_key_for_test("sold");
    h.get_by_label("Add partition").click();
    h.run_steps(2);
    h.get_by_label("Add MAXVALUE partition").click();
    h.run_steps(2);
    // The first partition's bound (a hint text isn't a label to find).
    h.state_mut()
        .test_table_editor()
        .edit_partitioning_for_test(|p| p.partitions[0].bound = "DATE '2026-01-01'".into());
    h.run_steps(3);
    save(&mut h, "table-builder-partitions");
    let sql = h
        .state_mut()
        .test_table_editor()
        .statements_for_test()
        .unwrap();
    assert!(
        sql[0].contains("partition by range (sold)\n(\n  partition p1 values less than (DATE '2026-01-01'),\n  partition p_max values less than (MAXVALUE)\n)"),
        "{}",
        sql[0]
    );
    let effects = h.state_mut().test_table_editor().apply_for_test();
    h.state_mut().test_apply_focused(effects);
    h.run_steps(2);
    wait_until(&mut h, "apply", |app| !app.test_focused_busy());
    let editor = h.state_mut().test_table_editor();
    assert_eq!(editor.outcome(), Some(("Applied", true)));
    assert_eq!(editor.partition_names_for_test(), ["P1", "P_MAX"]);
    h.run_steps(3);
    save(&mut h, "table-editor-partitions");

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: drop.into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// Procedures, packages and triggers start from a form whose source opens
/// in a program window; compiling there creates them.
#[test]
fn new_program_units() {
    use crate::object_editor::EditTarget;
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let cleanup = "begin
  for o in (select object_name, object_type from user_objects
             where object_name like 'PYTHIA\\_NPU\\_%' escape '\\'
               and object_type in ('TABLE', 'PACKAGE', 'PROCEDURE')) loop
    execute immediate 'drop ' || o.object_type || ' ' || o.object_name
      || case o.object_type when 'TABLE' then ' purge' end;
  end loop;
end;
/
";
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: format!(
            "{cleanup}create table pythia_npu_orders (id number primary key, status varchar2(10));"
        ),
        run: true,
    });
    wait_until(&mut h, "setup", |app| !app.test_focused_busy());
    let table = ObjectRef {
        owner: "PYTHIA".into(),
        name: "PYTHIA_NPU_ORDERS".into(),
        object_type: pythia_db::ObjectType::Table,
    };
    let open = |h: &mut Harness<'_, PythiaApp>, object_type, from: Option<ObjectRef>| {
        h.state_mut().test_open(OpenRequest::EditObject {
            connection: "pythia@FREEPDB1".into(),
            target: EditTarget::New {
                owner: "PYTHIA".into(),
                object_type,
                from,
            },
        });
        h.run_steps(2);
        wait_until(h, "form", |app| !app.test_focused_busy());
    };
    // Hands the form's source to a program window, which compiles it.
    let create = |h: &mut Harness<'_, PythiaApp>, name: &str| {
        let form = h.state().test_focused();
        let effects = h
            .state_mut()
            .test_object_editor()
            .open_in_editor_for_test("pythia@FREEPDB1");
        assert!(effects.close);
        h.state_mut().test_apply_focused(effects);
        h.run_steps(2);
        assert_ne!(h.state().test_focused(), form, "the form closed");
        let program = h.state_mut().test_program();
        assert!(program.new && program.is_dirty());
        assert_eq!(program.title(), name);
        let effects = h.state_mut().test_program().compile(true, false);
        h.state_mut().test_apply_focused(effects);
        h.run_steps(2);
        wait_until(h, "compile", |app| !app.test_focused_busy());
        let program = h.state_mut().test_program();
        let message = program.message();
        assert!(matches!(&message, Some((false, _))), "{name}: {message:?}");
        assert!(!program.new && !program.is_dirty());
    };

    // A trigger on the table: the table is filled in.
    open(&mut h, pythia_db::ObjectType::Trigger, Some(table.clone()));
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d
            .downcast_mut::<crate::object_editor::TriggerDef>()
            .unwrap();
        assert_eq!(d.table, "PYTHIA_NPU_ORDERS");
        d.update_columns = "status".into();
    });
    h.run_steps(3);
    save(&mut h, "new-trigger");
    create(&mut h, "pythia_npu_orders_biu");
    save(&mut h, "new-trigger-compiled");

    // A package with a procedure: spec and body with a stub.
    open(&mut h, pythia_db::ObjectType::Package, None);
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d
            .downcast_mut::<crate::object_editor::ProgramDef>()
            .unwrap();
        d.name = "pythia_npu_api".into();
        d.subprograms.push(crate::object_editor::Subprogram {
            function: false,
            name: "close_order".into(),
            params: vec![crate::object_editor::Param {
                name: "p_id".into(),
                mode: crate::object_editor::Mode::In,
                data_type: "pythia_npu_orders.id%type".into(),
                default: String::new(),
            }],
            returns: String::new(),
        });
    });
    h.run_steps(3);
    save(&mut h, "new-package");
    create(&mut h, "pythia_npu_api");

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: cleanup.into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// The other forms for new objects open filled in from what they're on
/// (a view, index or synonym for a table) or with sensible defaults, and
/// say what's missing before they turn into statements.
#[test]
fn new_object_forms() {
    use crate::object_editor::EditTarget;
    use pythia_db::ObjectType;
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin execute immediate 'drop table pythia_nof_items purge';
  exception when others then null; end;
/
create table pythia_nof_items (id number primary key, label varchar2(40), price number(10,2));"
            .into(),
        run: true,
    });
    wait_until(&mut h, "setup", |app| !app.test_focused_busy());
    let table = ObjectRef {
        owner: "PYTHIA".into(),
        name: "PYTHIA_NOF_ITEMS".into(),
        object_type: ObjectType::Table,
    };
    let open =
        |h: &mut Harness<'_, PythiaApp>, owner: &str, object_type, from: Option<ObjectRef>| {
            h.state_mut().test_open(OpenRequest::EditObject {
                connection: "pythia@FREEPDB1".into(),
                target: EditTarget::New {
                    owner: owner.into(),
                    object_type,
                    from,
                },
            });
            h.run_steps(2);
            wait_until(h, "form", |app| !app.test_focused_busy());
            h.run_steps(2);
            h.state_mut().test_object_editor().statements_for_test()
        };

    let view = open(&mut h, "PYTHIA", ObjectType::View, Some(table.clone())).unwrap();
    assert_eq!(
        view,
        [
            "create or replace view PYTHIA.PYTHIA_NOF_ITEMS_V as\nselect id,\n       label,\n       price\n  from pythia_nof_items"
        ]
    );
    save(&mut h, "new-view-on-table");
    let index = open(&mut h, "PYTHIA", ObjectType::Index, Some(table.clone()));
    assert_eq!(index, Err("Add at least one column".into()));
    let synonym = open(&mut h, "PYTHIA", ObjectType::Synonym, Some(table.clone())).unwrap();
    assert_eq!(
        synonym,
        ["create or replace public synonym PYTHIA_NOF_ITEMS for PYTHIA.PYTHIA_NOF_ITEMS"]
    );
    let mview = open(&mut h, "PYTHIA", ObjectType::MaterializedView, None);
    assert_eq!(mview, Err("Enter a name".into()));
    save(&mut h, "new-materialized-view");
    let link = open(&mut h, "PYTHIA", ObjectType::DatabaseLink, None);
    assert_eq!(link, Err("Enter a name".into()));
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d.downcast_mut::<crate::object_editor::LinkDef>().unwrap();
        d.name = "remote_hr".into();
        d.using = "db2:1521/hr".into();
        d.user = "hr".into();
        d.password = "secret".into();
    });
    h.run_steps(3);
    save(&mut h, "new-database-link");
    let public_link = open(&mut h, "PUBLIC", ObjectType::DatabaseLink, None);
    assert_eq!(public_link, Err("Enter a name".into()));
    let user = open(&mut h, "", ObjectType::User, None);
    assert_eq!(user, Err("Enter a name".into()));
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d
            .downcast_mut::<crate::object_editor::AccountDef>()
            .unwrap();
        d.name = "app_reader".into();
        d.password = "Secret_1".into();
        d.quotas = vec![("USERS".into(), "100M".into())];
        d.default_tablespace = "USERS".into();
        d.roles.insert("CONNECT".into(), Default::default());
        d.objects.insert(
            ("PYTHIA".into(), "PYTHIA_NOF_ITEMS".into(), "SELECT".into()),
            false,
        );
    });
    h.run_steps(3);
    save(&mut h, "new-user");
    let s = h
        .state_mut()
        .test_object_editor()
        .statements_for_test()
        .unwrap();
    assert!(s[0].starts_with("create user APP_READER"), "{s:?}");
    assert_eq!(h.state().test_focused_title().as_deref(), Some("New user"));
    let role = open(&mut h, "", ObjectType::Role, None);
    assert_eq!(role, Err("Enter a name".into()));
    let new_directory = open(&mut h, "", ObjectType::Directory, None);
    assert_eq!(new_directory, Err("Enter a name".into()));
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d
            .downcast_mut::<crate::object_editor::DirectoryDef>()
            .unwrap();
        d.name = "exports".into();
        d.path = "/u01/app/exports".into();
    });
    h.run_steps(3);
    save(&mut h, "new-directory");
    assert_eq!(
        h.state_mut()
            .test_object_editor()
            .statements_for_test()
            .unwrap(),
        ["create or replace directory EXPORTS as '/u01/app/exports'"]
    );
    assert!(
        h.query_by_label_contains("doesn't have CREATE ANY DIRECTORY")
            .is_some()
    );
    let new_profile = open(&mut h, "", ObjectType::Profile, None);
    assert_eq!(new_profile, Err("Enter a name".into()));
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d
            .downcast_mut::<crate::object_editor::ProfileDef>()
            .unwrap();
        d.name = "app_users".into();
        for l in &mut d.limits {
            match l.resource.as_str() {
                "FAILED_LOGIN_ATTEMPTS" => l.value = "5".into(),
                "PASSWORD_LIFE_TIME" => l.value = "90".into(),
                "SESSIONS_PER_USER" => l.value = "2".into(),
                _ => {}
            }
        }
    });
    h.run_steps(3);
    save(&mut h, "new-profile");
    let s = h
        .state_mut()
        .test_object_editor()
        .statements_for_test()
        .unwrap();
    assert_eq!(
        s,
        [
            "create profile APP_USERS limit\n  failed_login_attempts 5\n  \
          password_life_time 90\n  sessions_per_user 2"
        ]
    );
    assert_eq!(
        h.state().test_focused_title().as_deref(),
        Some("New profile")
    );
    let ty = open(&mut h, "PYTHIA", ObjectType::Type, None);
    assert_eq!(ty, Err("Enter a name".into()));
    let procedure = open(&mut h, "PYTHIA", ObjectType::Procedure, None);
    assert_eq!(procedure, Err("Enter a name".into()));
    // A name that's taken: the form says compiling replaces it.
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "create or replace procedure pythia_nof_proc is begin null; end;\n/".into(),
        run: true,
    });
    wait_until(&mut h, "procedure", |app| !app.test_focused_busy());
    open(&mut h, "PYTHIA", ObjectType::Procedure, None).ok();
    h.state_mut().test_object_editor().edit_for_test(|d| {
        let d = d
            .downcast_mut::<crate::object_editor::ProgramDef>()
            .unwrap();
        d.name = "pythia_nof_proc".into();
    });
    h.run_steps(3);
    assert!(
        h.query_by_label_contains("compiling the new source replaces it")
            .is_some()
    );
    save(&mut h, "new-procedure");

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop table pythia_nof_items purge;\ndrop procedure pythia_nof_proc;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// The object browser's right-click menus open the forms: New table… on
/// the Tables folder, New ▸ Index on this table… on a table, New user… on
/// Users.
#[test]
fn browser_new_menus() {
    use crate::object_editor::EditTarget;
    use pythia_db::ObjectType;
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = headless_harness();
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin execute immediate 'drop table pythia_bnm_t purge';
  exception when others then null; end;
/
create table pythia_bnm_t (id number primary key);"
            .into(),
        run: true,
    });
    wait_until(&mut h, "setup", |app| !app.test_focused_busy());
    // Directories show their path; Grants… is in their menu.
    let dba = std::env::var("PYTHIA_TEST_DB_DBA").ok().map(|spec| {
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        pythia_db::oracle::Connection::connect(user, pw, db).unwrap()
    });
    if let Some(dba) = &dba {
        dba.execute(
            "create or replace directory pythia_bnm_dir as '/tmp/pythia_bnm'",
            &[],
        )
        .unwrap();
        dba.execute("grant read on directory pythia_bnm_dir to pythia", &[])
            .unwrap();
        h.state_mut().test_browser().set_filter("PYTHIA_BNM_DIR");
        h.run_steps(2);
        wait_until(&mut h, "directories", |app| !app.test_browser_loading());
        h.get_by_label("Directories").click();
        h.run_steps(2);
        wait_until(&mut h, "directories", |app| !app.test_browser_loading());
        assert!(h.query_by_label("/tmp/pythia_bnm").is_some());
        save(&mut h, "browser-directories");
        h.get_by_label("pythia_bnm_dir").click_secondary();
        h.run_steps(2);
        assert!(h.query_by_label("Edit directory…").is_some());
        h.get_by_label("Grants…").click();
        h.run_steps(2);
        wait_until(&mut h, "grants", |app| !app.test_focused_busy());
        assert_eq!(
            h.state().test_focused_title().as_deref(),
            Some("pythia_bnm_dir grants")
        );
        assert!(
            h.query_by_label("Grants on directory PYTHIA_BNM_DIR")
                .is_some()
        );
        dba.execute("drop directory pythia_bnm_dir", &[]).unwrap();
    }
    // Users and roles are listed below the folders.
    h.state_mut().test_browser().set_filter("PYTHIA");
    h.run_steps(2);
    h.get_by_label("Users").click_secondary();
    h.run_steps(2);
    h.get_by_label("New user…").click();
    h.run_steps(2);
    wait_until(&mut h, "user form", |app| !app.test_focused_busy());
    assert_eq!(h.state().test_focused_title().as_deref(), Some("New user"));
    h.get_by_label("Users").click();
    h.run_steps(2);
    wait_until(&mut h, "users", |app| !app.test_browser_loading());
    h.get_by_label("pythia").click_secondary();
    h.run_steps(2);
    assert!(h.query_by_label("Edit user…").is_some());
    assert!(h.query_by_label("Show schema").is_some());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    // So are profiles; DEFAULT can't be dropped.
    h.state_mut().test_browser().set_filter("DEFAULT");
    h.run_steps(2);
    h.get_by_label("Profiles").click();
    h.run_steps(2);
    wait_until(&mut h, "profiles", |app| !app.test_browser_loading());
    h.get_by_label("default").click_secondary();
    h.run_steps(2);
    assert!(h.query_by_label("New profile…").is_some());
    assert!(h.query_by_label("Drop…").is_none());
    h.get_by_label("Edit profile…").click();
    h.run_steps(2);
    wait_until(&mut h, "profile form", |app| !app.test_focused_busy());
    h.run_steps(2);
    assert_eq!(
        h.state().test_focused_title().as_deref(),
        Some("default (profile)")
    );
    save(&mut h, "edit-default-profile");

    h.state_mut().test_browser().set_filter("PYTHIA_BNM");
    h.run_steps(2);

    h.get_by_label("Tables").click_secondary();
    h.run_steps(2);
    save(&mut h, "browser-new-folder-menu");
    h.get_by_label("New table…").click();
    h.run_steps(2);
    wait_until(&mut h, "table builder", |app| !app.test_focused_busy());
    assert_eq!(h.state().test_focused_title().as_deref(), Some("New table"));

    h.state_mut().test_expand_browser(&[ObjectType::Table]);
    wait_until(&mut h, "tables", |app| !app.test_browser_loading());
    h.get_by_label("pythia_bnm_t").click_secondary();
    h.run_steps(2);
    // Submenus carry their arrow in the label.
    h.get_by_label("New ⏵").hover();
    h.run_steps(2);
    save(&mut h, "browser-new-submenu");
    h.get_by_label("Index on this table…").click();
    h.run_steps(2);
    wait_until(&mut h, "index form", |app| !app.test_focused_busy());
    let table = ObjectRef {
        owner: "PYTHIA".into(),
        name: "PYTHIA_BNM_T".into(),
        object_type: ObjectType::Table,
    };
    assert_eq!(
        h.state_mut().test_object_editor().target,
        EditTarget::New {
            owner: "PYTHIA".into(),
            object_type: ObjectType::Index,
            from: Some(table),
        }
    );

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop table pythia_bnm_t purge;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// The result grid's columns: dragging a header moves its column, the
/// header menu freezes columns (which stay in step when scrolling), and
/// copies follow the order shown.
#[test]
fn grid_columns() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "select level id, 'name ' || level name_col, level * 10 amount,
       date '2026-01-01' + level created
  from dual connect by level <= 40"
            .into(),
        run: true,
    });
    wait_until(&mut h, "query", |app| !app.test_focused_busy());
    h.run_steps(3);
    let order = |h: &Harness<'_, PythiaApp>| {
        h.state().test_sql_window_ref().unwrap().state.results[0]
            .grid
            .visible_columns(4)
    };
    assert_eq!(order(&h), [0, 1, 2, 3]);

    // Drag CREATED onto the left half of NAME_COL: it lands before it.
    let from = h.get_by_label("CREATED").rect().center();
    let to = h.get_by_label("NAME_COL").rect().left_center() + egui::vec2(3.0, 0.0);
    h.hover_at(from);
    h.run_steps(1);
    h.drag_at(from);
    h.run_steps(1);
    for step in 1..=4 {
        h.hover_at(from + (to - from) * (step as f32 / 4.0));
        h.run_steps(1);
    }
    h.run_steps(1);
    save(&mut h, "grid-column-drag");
    h.event(egui::Event::PointerButton {
        pos: to,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(2);
    h.remove_cursor();
    h.run_steps(2);
    assert_eq!(order(&h), [0, 3, 1, 2]);

    // Freeze up to CREATED (now second): ID and CREATED stay in place.
    h.get_by_label("CREATED").click_secondary();
    h.run_steps(2);
    h.get_by_label("Columns ⏵").hover();
    h.run_steps(2);
    save(&mut h, "grid-header-menu");
    h.get_by_label("Freeze up to CREATED").click();
    h.run_steps(3);
    let grid = &h.state().test_sql_window_ref().unwrap().state.results[0].grid;
    assert_eq!(grid.frozen_columns(4), 2);
    // CREATED kept its width (fitted to the dates) through the move and
    // into the frozen part. A label's rect is only what's shown, so measure
    // the column against 19 monospace characters (from a 6-character value).
    let date = h.get_by_label("2026-01-02 00:00:00").rect();
    let column = h.get_by_label("name 1").rect().left() - date.left();
    let needed = 19.0 * h.get_by_label("name 1").rect().width() / 6.0;
    assert!(column > needed, "the date is cut off: {column} < {needed}");
    save(&mut h, "grid-frozen");

    // Scrolling the other part down takes the frozen part along: row 40
    // shows in both (its number and ID left, its amount right).
    let at = h.get_by_label("NAME_COL").rect().center() + egui::vec2(0.0, 60.0);
    h.hover_at(at);
    for _ in 0..6 {
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -20.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        h.run_steps(2);
    }
    h.run_steps(10);
    save(&mut h, "grid-frozen-scrolled");
    assert!(h.query_by_label("400").is_some(), "the right part scrolled");
    assert!(
        h.query_all_by_label("40").count() >= 2,
        "row number and ID of row 40 in the frozen part"
    );

    // Copies and exports go by the order shown.
    let w = h.state().test_sql_window_ref().unwrap();
    let r = &w.state.results[0];
    let (columns, _) = pythia_ui::shown_data(&r.columns, &r.rows, &r.grid);
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["ID", "CREATED", "NAME_COL", "AMOUNT"]);
}

/// Closing a window with an uncommitted transaction asks first: Cancel
/// keeps it, Roll back and close or Commit and close end it (#35).
#[test]
fn close_with_open_transaction() {
    use crate::commands::Command;
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let run = |h: &mut Harness<'_, PythiaApp>, text: &str| {
        h.state_mut().test_open(OpenRequest::Sql {
            connection: "pythia@FREEPDB1".into(),
            text: text.into(),
            run: true,
        });
        wait_until(h, text, |app| !app.test_focused_busy());
    };
    run(
        &mut h,
        "begin execute immediate 'drop table pythia_ctx_t purge';
  exception when others then null; end;
/
create table pythia_ctx_t (id number);",
    );
    // What another session sees, once it's there.
    let count = |h: &mut Harness<'_, PythiaApp>, expected: &str| {
        let start = Instant::now();
        loop {
            run(h, "select count(*) from pythia_ctx_t");
            let seen = h.state().test_sql_window_ref().unwrap().state.results[0].rows[0][0]
                .clone()
                .unwrap_or_default();
            h.state_mut().test_command(Command::CloseTab);
            h.run_steps(2);
            if seen == expected || start.elapsed() > Duration::from_secs(10) {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };

    run(&mut h, "insert into pythia_ctx_t values (1)");
    let window = h.state().test_focused();
    assert!(h.state().test_pending_tx());
    h.state_mut().test_command(Command::CloseTab);
    h.run_steps(2);
    assert!(h.query_by_label("Commit and close").is_some());
    h.get_by_label_contains("has uncommitted changes");
    save(&mut h, "close-open-transaction");

    // Cancel: the window and its transaction stay.
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert_eq!(h.state().test_focused(), window);
    assert!(h.state().test_pending_tx());

    // Roll back and close.
    h.state_mut().test_command(Command::CloseTab);
    h.run_steps(2);
    h.get_by_label("Roll back and close").click();
    h.run_steps(2);
    assert!(!h.state().test_pending_tx());
    assert_ne!(h.state().test_focused(), window, "closed");
    assert_eq!(count(&mut h, "0"), "0");

    // Commit and close.
    run(&mut h, "insert into pythia_ctx_t values (2)");
    h.state_mut().test_command(Command::CloseTab);
    h.run_steps(2);
    h.get_by_label("Commit and close").click();
    h.run_steps(2);
    assert_eq!(count(&mut h, "1"), "1");

    run(&mut h, "drop table pythia_ctx_t purge");
}

/// Favorites, recently opened objects and the invalid-only filter.
#[test]
fn browser_marks() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let object = |name: &str, t| ObjectRef {
        owner: "PYTHIA".into(),
        name: name.into(),
        object_type: t,
    };
    h.state_mut().test_open(OpenRequest::Program {
        connection: "pythia@FREEPDB1".into(),
        object: object("DEMO_PKG", pythia_db::ObjectType::PackageBody),
    });
    wait_until(&mut h, "program", |app| !app.test_focused_busy());
    h.state_mut()
        .test_browser()
        .test_marks(object("DEMO_EMPLOYEES", pythia_db::ObjectType::Table));
    h.state_mut().test_expand_browser(&[
        pythia_db::ObjectType::Procedure,
        pythia_db::ObjectType::Package,
        pythia_db::ObjectType::PackageBody,
    ]);
    wait_until(&mut h, "browser", |app| !app.test_browser_loading());
    save(&mut h, "browser-marks");
    let marks = &h.state_mut().test_browser().marks;
    assert_eq!(marks.recent["pythia@FREEPDB1"][0].name, "DEMO_PKG");
    assert_eq!(marks.favorites["pythia@FREEPDB1"][0].name, "DEMO_EMPLOYEES");
}

/// Dragging a table from the browser into the editor inserts its name.
#[test]
fn drag_object_into_editor() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "select * from ".into(),
        run: false,
    });
    h.state_mut()
        .test_expand_browser(&[pythia_db::ObjectType::Table]);
    wait_until(&mut h, "browser", |app| !app.test_browser_loading());
    h.run_steps(3);
    let from = h.get_by_label("demo_employees").rect().center();
    let to = egui::pos2(900.0, 250.0);
    h.hover_at(from);
    h.run_steps(1);
    h.drag_at(from);
    h.run_steps(1);
    for i in 1..=10 {
        h.hover_at(from + (to - from) * (i as f32 / 10.0));
        h.run_steps(1);
    }
    h.drop_at(to);
    h.run_steps(3);
    let text = h.state_mut().test_sql_window().editor.text().to_owned();
    assert!(text.contains("demo_employees"), "{text:?}");
    // Rows still take clicks.
    h.get_by_label("demo_departments").click();
    h.run_steps(2);
    assert_eq!(
        h.state_mut()
            .test_browser()
            .selected()
            .map(|o| o.name.as_str()),
        Some("DEMO_DEPARTMENTS")
    );
}

/// Debug… on a subprogram's name in a program window: in the spec and in
/// the body for what the spec declares, not for the body's private
/// subprograms. It opens a SQL window with a block that calls the
/// procedure and debugs it right away: the body gets debug information and
/// the run stops on entry to the procedure, once.
#[test]
fn debug_from_package_spec() {
    let (Some((profile, password)), Ok(host)) = (profile(), std::env::var("PYTHIA_DEBUG_HOST"))
    else {
        eprintln!("PYTHIA_TEST_DB / PYTHIA_DEBUG_HOST not set, skipping");
        return;
    };
    let setup = pythia_debugger::target::setup_statements(&profile.username, &host);
    crate::test_db::as_dba(&setup.iter().map(String::as_str).collect::<Vec<_>>());
    let spec = std::env::var("PYTHIA_TEST_DB").unwrap();
    let (cred, db) = spec.rsplit_once('@').unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
    for ddl in [
        "create or replace package pythia_entry_pkg as\n  procedure run;\nend;",
        "create or replace package body pythia_entry_pkg as\n  \
         function helper(p_n number) return number is\n  begin\n    return p_n * 2;\n  end;\n\n  \
         procedure run is\n    l_total number := 0;\n  begin\n    for i in 1 .. 3 loop\n      \
         l_total := l_total + helper(i);\n    end loop;\n  end;\nend;",
    ] {
        conn.execute(ddl, &[]).unwrap();
    }
    use pythia_debugger::{StopReason, Unit, UnitType};
    let body = Unit::new("PYTHIA", "PYTHIA_ENTRY_PKG", UnitType::PackageBody);
    assert!(!pythia_debugger::target::has_debug_info(&conn, &body).unwrap());

    let mut h = harness(true);
    h.state_mut().test_settings_mut().debug_host = host;
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Program {
        connection: "pythia@FREEPDB1".into(),
        object: ObjectRef {
            owner: "PYTHIA".into(),
            name: "PYTHIA_ENTRY_PKG".into(),
            object_type: pythia_db::ObjectType::Package,
        },
    });
    wait_until(&mut h, "source", |app| !app.test_focused_busy());
    let right_click = |h: &mut Ui, word: &str| {
        let editor = h.state_mut().test_program_editor().expect("program editor");
        let offset = editor.text().find(word).expect(word) + 2;
        let pos = editor.inner().offset_pos(offset).expect("drawn");
        h.event(egui::Event::PointerMoved(pos));
        h.step();
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: Default::default(),
            });
            h.step();
        }
    };
    // Waits until the menu shows `label`, maybe with a shortcut (the outline
    // is worked out shortly after the source loaded).
    let menu_item = |h: &mut Ui, label: &str| {
        let start = Instant::now();
        while h.query_by_label_contains(label).is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "no menu item {label}"
            );
            h.step();
            std::thread::sleep(Duration::from_millis(15));
        }
    };

    // The body: its public procedure, not the private helper.
    h.state_mut().test_program().active = 1;
    h.run_steps(2);
    right_click(&mut h, "run is");
    menu_item(&mut h, "Debug…");
    assert!(h.query_by_label("Test…").is_some());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    right_click(&mut h, "helper(p_n");
    menu_item(&mut h, "Copy");
    assert!(h.query_by_label("Debug…").is_none(), "private");
    h.key_press(egui::Key::Escape);

    // The spec: Debug… on the procedure.
    h.state_mut().test_program().active = 0;
    h.run_steps(2);
    right_click(&mut h, "run;");
    menu_item(&mut h, "Debug…");
    save(&mut h, "debug-from-spec-menu");
    h.get_by_label("Debug…").click();
    wait_until(&mut h, "stop on entry", |app| {
        app.test_debug().is_suspended()
    });
    let debug = h.state().test_debug();
    assert_eq!(
        debug.state,
        pythia_debugger::ui::State::Suspended {
            reason: StopReason::Entry
        },
        "{:?}",
        debug.log
    );
    assert_eq!(debug.stack[0].location.unit, body, "{:?}", debug.stack);
    assert_eq!(debug.stack[0].method, "RUN");
    assert_eq!(
        debug.stack[0].location.line,
        Some(7),
        "the procedure's header"
    );
    assert!(pythia_debugger::target::has_debug_info(&conn, &body).unwrap());
    assert_eq!(debug.entry, Some((body.clone(), "RUN".to_owned())));
    h.state_mut()
        .test_debug_mut()
        .command(pythia_debugger::DebugCommand::Resume);
    wait_until(&mut h, "finish", |app| !app.test_debug_running());

    conn.execute("drop package pythia_entry_pkg", &[]).unwrap();
}

/// Right-clicking a name in an editor offers what the object browser offers
/// for it, without Drop; package members open at the subprogram.
#[test]
fn editor_context_menu() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let spec = std::env::var("PYTHIA_TEST_DB").unwrap();
    let (cred, db) = spec.rsplit_once('@').unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
    let _ = conn.execute("drop table pythia_menu_t purge", &[]);
    for ddl in [
        "create table pythia_menu_t (id number)",
        "create or replace package pythia_menu_pkg as\n  procedure do_it;\nend;",
        "create or replace package body pythia_menu_pkg as\n  procedure helper is begin null; end;\n  procedure do_it is\n  begin\n    helper;\n  end;\nend;",
    ] {
        conn.execute(ddl, &[]).unwrap();
    }
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let text = "select * from pythia_menu_t;\nbegin\n  pythia_menu_pkg.do_it;\nend;\n";
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: text.into(),
        run: false,
    });
    h.run_steps(3);
    let right_click = |h: &mut Ui, word: &str| {
        let offset = text.find(word).unwrap() + 2;
        let pos = h
            .state_mut()
            .test_sql_window()
            .editor
            .inner()
            .offset_pos(offset)
            .expect("drawn");
        h.event(egui::Event::PointerMoved(pos));
        h.step();
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: Default::default(),
            });
            h.step();
        }
    };
    // Waits until the menu shows `label` (names are looked up in the background).
    let menu_item = |h: &mut Ui, label: &str| {
        let start = Instant::now();
        while h.query_by_label(label).is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "no menu item {label}"
            );
            h.step();
            std::thread::sleep(Duration::from_millis(15));
        }
    };

    right_click(&mut h, "pythia_menu_t");
    menu_item(&mut h, "Query data");
    for label in ["Edit table…", "Describe", "Show in object browser"] {
        assert!(h.query_by_label(label).is_some(), "{label}");
    }
    // With a submenu arrow or a shortcut in the label.
    for label in ["Generate", "Cut", "Paste", "Toggle comment"] {
        assert!(h.query_by_label_contains(label).is_some(), "{label}");
    }
    assert!(h.query_by_label("Drop…").is_none(), "no Drop in editors");
    save(&mut h, "17-editor-context-menu-dark");
    h.get_by_label("Show in object browser").click();
    let table = ObjectRef {
        owner: "PYTHIA".into(),
        name: "PYTHIA_MENU_T".into(),
        object_type: pythia_db::ObjectType::Table,
    };
    wait_until(&mut h, "revealed", |app| {
        app.test_browser_selected().as_ref() == Some(&table)
    });

    right_click(&mut h, "do_it");
    menu_item(&mut h, "Edit do_it");
    assert!(h.query_by_label("Test…").is_some());
    h.get_by_label("Edit do_it").click();
    let start = Instant::now();
    loop {
        h.step();
        std::thread::sleep(Duration::from_millis(15));
        if let Some(editor) = h.state_mut().test_program_editor()
            && let Some(range) = editor.selection()
        {
            assert_eq!(&editor.text()[range.clone()], "do_it");
            assert!(
                editor
                    .text()
                    .to_lowercase()
                    .starts_with("create or replace package body")
            );
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "package body not opened at do_it"
        );
    }
    conn.execute("drop package pythia_menu_pkg", &[]).unwrap();
    conn.execute("drop table pythia_menu_t purge", &[]).unwrap();
}

/// Go to definition, Back/Forward, Switch Spec/Body, Go to Symbol and Find
/// References in a package's program window, across its spec and body.
#[test]
fn navigation_across_spec_and_body() {
    use crate::commands::Command;
    let spec = "create or replace package nav_pkg as\n  -- The limit.\n  g_limit number := 10;\n  \
                procedure raise(p_id number, p_pct number);\n  \
                procedure raise(p_id number, p_amount number, p_reason varchar2);\nend nav_pkg;";
    let body = "create or replace package body nav_pkg as\n  \
                procedure raise(p_id number, p_pct number) is\n  begin\n    \
                if p_pct > g_limit then\n      raise(p_id, p_pct, 'too much');\n    end if;\n  end;\n  \
                procedure raise(p_id number, p_amount number, p_reason varchar2) is\n  begin\n    \
                null;\n  end;\nend nav_pkg;";
    let mut h = harness(true);
    h.state_mut().test_program_with(
        ObjectRef {
            owner: "PYTHIA".into(),
            name: "NAV_PKG".into(),
            object_type: pythia_db::ObjectType::PackageBody,
        },
        spec,
        body,
    );
    h.run_steps(3);
    // (active part, selection or caret) of the program window.
    let state = |h: &mut Ui| {
        let w = h.state_mut().test_program();
        let part = w.active;
        let editor = w.active_editor().unwrap();
        (
            part,
            editor
                .selection()
                .unwrap_or(editor.cursor()..editor.cursor()),
        )
    };
    let place = |h: &mut Ui, part: usize, at: usize| {
        h.state_mut().test_program().goto_position(part, at..at);
        h.run_steps(2);
    };
    let run = |h: &mut Ui, command: Command| {
        h.state_mut().test_command(command);
        h.run_steps(3);
    };

    // A spec variable used in the body → its declaration in the spec.
    let use_at = body.find("g_limit").unwrap() + 2;
    place(&mut h, 1, use_at);
    run(&mut h, Command::GoToDefinition);
    let decl = spec.find("g_limit").unwrap();
    assert_eq!(state(&mut h), (0, decl..decl + 7));
    run(&mut h, Command::NavigateBack);
    assert_eq!(state(&mut h), (1, use_at..use_at));
    run(&mut h, Command::NavigateForward);
    assert_eq!(state(&mut h).0, 0);

    // Peek shows the declaration (and its comment) in place.
    place(&mut h, 1, use_at);
    run(&mut h, Command::PeekDefinition);
    assert_eq!(state(&mut h), (1, use_at..use_at), "peeking doesn't move");
    assert!(h.query_by_label_contains("spec, line 3").is_some());

    // The overload the call's arguments fit.
    let call = body.find("raise(p_id, p_pct, 'too much')").unwrap() + 1;
    place(&mut h, 1, call);
    run(&mut h, Command::GoToDefinition);
    let second = body.rfind("procedure raise").unwrap() + "procedure ".len();
    assert_eq!(state(&mut h), (1, second..second + 5));

    // Switch to the spec at the same overload.
    place(&mut h, 1, body.rfind("null;").unwrap());
    run(&mut h, Command::SwitchSpecBody);
    let in_spec = spec.rfind("procedure raise").unwrap() + "procedure ".len();
    assert_eq!(state(&mut h), (0, in_spec..in_spec + 5));

    // Go to Symbol lists both parts.
    run(&mut h, Command::GoToSymbol);
    assert!(h.query_by_label_contains("variable · Spec").is_some());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);

    // Find References: in both parts, stepped through with the arrows;
    // Escape goes back.
    place(&mut h, 1, use_at);
    run(&mut h, Command::FindReferences);
    assert!(h.state().test_reference_rows());
    assert!(
        h.query_by_label_contains("if p_pct > g_limit then")
            .is_some()
    );
    assert!(h.query_by_label_contains("g_limit number := 10;").is_some());
    let first = state(&mut h);
    h.key_press(egui::Key::ArrowDown);
    h.run_steps(2);
    assert_ne!(state(&mut h), first, "stepping shows the next reference");
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(!h.state().test_reference_rows());
    assert_eq!(state(&mut h), (1, use_at..use_at), "back where it started");
}

/// The outline's context menu in a package's program window: Go to body
/// and Go to specification land on the same overload, whatever order the
/// body has (Back returns), Find references, Copy name. Without a
/// connection there's nothing to test.
#[test]
fn outline_context_menu() {
    use crate::commands::Command;
    let spec = "create or replace package outline_pkg as\n  g_limit number := 10;\n  \
                procedure raise(p_id number, p_pct number);\n  \
                procedure raise(p_id number, p_amount number, p_reason varchar2);\nend outline_pkg;";
    let body = "create or replace package body outline_pkg as\n  \
                procedure raise(p_id number, p_amount number, p_reason varchar2) is\n  begin\n    \
                null;\n  end;\n  \
                procedure raise(p_id number, p_pct number) is\n  begin\n    \
                if p_pct > g_limit then\n      raise(p_id, p_pct, 'too much');\n    end if;\n  end;\n\
                end outline_pkg;";
    let mut h = harness(true);
    h.state_mut().test_program_with(
        ObjectRef {
            owner: "PYTHIA".into(),
            name: "OUTLINE_PKG".into(),
            object_type: pythia_db::ObjectType::Package,
        },
        spec,
        body,
    );
    h.run_steps(3);
    // (active part, selection or caret) of the program window.
    let state = |h: &mut Ui| {
        let w = h.state_mut().test_program();
        let part = w.active;
        let editor = w.active_editor().unwrap();
        (
            part,
            editor
                .selection()
                .unwrap_or(editor.cursor()..editor.cursor()),
        )
    };
    let menu = |h: &mut Ui, name: &str, nth: usize| {
        h.get_all_by_label(name)
            .nth(nth)
            .expect(name)
            .click_secondary();
        h.run_steps(2);
    };

    // The spec's second overload is the body's first.
    assert_eq!(state(&mut h).0, 0);
    menu(&mut h, "raise", 1);
    assert!(h.query_by_label("Find references").is_some());
    assert!(h.query_by_label("Test…").is_none(), "not connected");
    save(&mut h, "outline-context-menu");
    h.get_by_label("Go to body").click();
    h.run_steps(3);
    let first = body.find("raise").unwrap();
    assert_eq!(state(&mut h), (1, first..first + 5));
    h.state_mut().test_command(Command::NavigateBack);
    h.run_steps(3);
    assert_eq!(state(&mut h).0, 0, "Back returns to the spec");

    // And back from the body's second to the spec's first.
    h.state_mut().test_program().active = 1;
    h.run_steps(3);
    menu(&mut h, "raise", 1);
    h.get_by_label("Go to specification").click();
    h.run_steps(3);
    let first = spec.find("raise").unwrap();
    assert_eq!(state(&mut h), (0, first..first + 5));

    // References to a variable, in both parts.
    menu(&mut h, "g_limit", 0);
    assert!(
        h.query_by_label("Go to body").is_none(),
        "only for subprograms"
    );
    h.get_by_label("Find references").click();
    h.run_steps(3);
    assert!(h.state().test_reference_rows());
    assert!(
        h.query_by_label_contains("if p_pct > g_limit then")
            .is_some()
    );
    h.key_press(egui::Key::Escape);
    h.run_steps(3);

    menu(&mut h, "g_limit", 0);
    h.get_by_label("Copy name").click();
    h.step();
    let copied = h
        .output()
        .platform_output
        .commands
        .iter()
        .any(|c| matches!(c, egui::OutputCommand::CopyText(text) if text == "g_limit"));
    assert!(copied, "{:?}", h.output().platform_output.commands);
}

/// Test… and Debug… from the outline of a package body: for the
/// subprograms its spec declares, disabled for a private one. Test… opens
/// the block that calls it.
#[test]
fn test_from_outline() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let spec = std::env::var("PYTHIA_TEST_DB").unwrap();
    let (cred, db) = spec.rsplit_once('@').unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
    for ddl in [
        "create or replace package pythia_outline_pkg as\n  procedure run(p_n number);\nend;",
        "create or replace package body pythia_outline_pkg as\n  \
         function helper(p_n number) return number is\n  begin\n    return p_n * 2;\n  end;\n\n  \
         procedure run(p_n number) is\n    l_total number;\n  begin\n    \
         l_total := helper(p_n);\n  end;\nend;",
    ] {
        conn.execute(ddl, &[]).unwrap();
    }
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Program {
        connection: "pythia@FREEPDB1".into(),
        object: ObjectRef {
            owner: "PYTHIA".into(),
            name: "PYTHIA_OUTLINE_PKG".into(),
            object_type: pythia_db::ObjectType::PackageBody,
        },
    });
    wait_until(&mut h, "source", |app| !app.test_focused_busy());
    // The outline is worked out shortly after the source loaded.
    let start = Instant::now();
    while h.query_by_label("helper").is_none() {
        assert!(start.elapsed() < Duration::from_secs(30), "no outline");
        h.step();
        std::thread::sleep(Duration::from_millis(15));
    }
    let disabled = |h: &Ui, label: &str| {
        use egui_kittest::kittest::NodeT;
        h.get_by_label(label).accesskit_node().is_disabled()
    };

    h.get_by_label("helper").click_secondary();
    h.run_steps(2);
    assert!(disabled(&h, "Test…"), "private");
    assert!(disabled(&h, "Debug…"), "private");
    h.key_press(egui::Key::Escape);
    h.run_steps(2);

    h.get_by_label("run").click_secondary();
    h.run_steps(2);
    assert!(!disabled(&h, "Debug…"));
    save(&mut h, "test-from-outline");
    h.get_by_label("Test…").click();
    wait_until(&mut h, "test block", |app| {
        app.test_sql_window_ref()
            .is_some_and(|w| w.editor.text().contains("pythia_outline_pkg.run("))
    });

    conn.execute("drop package pythia_outline_pkg", &[])
        .unwrap();
}

/// The status bar counts invalid objects; its indicator opens the invalid
/// objects window, which recompiles them over two sessions.
#[test]
fn invalid_objects_indicator_and_recompile() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let spec = std::env::var("PYTHIA_TEST_DB").unwrap();
    let (cred, db) = spec.rsplit_once('@').unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
    let drops = [
        "drop package inv_bad",
        "drop package inv_p",
        "drop view inv_v2",
        "drop view inv_v1",
        "drop table inv_t purge",
    ];
    for sql in drops {
        let _ = conn.execute(sql, &[]);
    }
    for sql in [
        "create table inv_t (a number, b number)",
        "create or replace view inv_v1 as select a, b from inv_t",
        "create or replace view inv_v2 as select a from inv_v1",
        "create or replace package inv_p as function n return number; end;",
        "create or replace package body inv_p as function n return number is l number; \
         begin select count(*) into l from inv_v2; return l; end; end;",
        "create or replace package inv_bad as procedure p; end;",
        "create or replace package body inv_bad as procedure p is l number; \
         begin select b into l from inv_v2; end; end;",
        "drop table inv_t purge",
        "create table inv_t (a number, b number)",
    ] {
        let _ = conn.execute(sql, &[]);
    }
    let invalid_now = || -> u32 {
        conn.query_row_as::<u32>(
            "select count(*) from user_objects where status = 'INVALID' and object_name like 'INV\\_%' escape '\\'",
            &[],
        )
        .unwrap()
    };
    assert!(invalid_now() >= 4, "set up invalid objects");
    // Other tests' leftovers count too; ours make a difference of at least 4.
    let mut h = harness(true).alone();
    h.state_mut().test_settings_mut().invalid.parallelism = 2;
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    wait_until(&mut h, "invalid count", |app| {
        app.test_invalid_total("pythia@FREEPDB1").is_some()
    });
    let total = h.state().test_invalid_total("pythia@FREEPDB1").unwrap();
    assert!(total >= 4, "{total}");
    h.run_steps(2);
    h.get_by_label_contains(&format!("{total} invalid")).click();
    wait_until(&mut h, "invalid objects window", |app| {
        app.test_invalid_window()
            .is_some_and(|w| !w.is_busy() && !w.objects().is_empty())
    });
    // Other tests make objects invalid and valid again meanwhile: only
    // this test's objects hold still.
    let ours = h
        .state()
        .test_invalid_window()
        .unwrap()
        .objects()
        .iter()
        .filter(|o| o.id.name.starts_with("INV_"))
        .count();
    assert_eq!(ours as u32, invalid_now());
    let bad = h
        .state()
        .test_invalid_window()
        .unwrap()
        .objects()
        .iter()
        .find(|o| o.id.name == "INV_BAD" && o.id.object_type == "PACKAGE BODY")
        .cloned()
        .expect("listed");
    assert!(bad.compilable);
    h.run_steps(2);
    save(&mut h, "18-invalid-objects-dark");
    h.get_by_label_contains("Recompile all").click();
    wait_until(&mut h, "recompiled", |app| {
        app.test_invalid_window()
            .is_some_and(|w| !w.is_busy() && w.result().is_some_and(|r| r.contains("valid now")))
    });
    let result = h
        .state()
        .test_invalid_window()
        .unwrap()
        .result()
        .unwrap()
        .to_owned();
    eprintln!("{result}");
    assert_eq!(invalid_now(), 1, "only inv_bad's body is left: {result}");
    let left: Vec<String> = h
        .state()
        .test_invalid_window()
        .unwrap()
        .objects()
        .iter()
        .map(|o| o.id.name.clone())
        .collect();
    assert!(left.contains(&"INV_BAD".to_owned()), "{left:?}");
    assert!(!left.contains(&"INV_P".to_owned()), "{left:?}");
    // The status bar follows (ours went from at least 4 to 1).
    wait_until(&mut h, "count refreshed", |app| {
        app.test_invalid_total("pythia@FREEPDB1")
            .is_some_and(|n| n < total)
    });
    for sql in drops {
        conn.execute(sql, &[]).unwrap();
    }
}

/// Compiling in a program window updates the status bar's invalid count:
/// with Compile all, as it is after the body, not after the spec.
#[test]
fn program_compile_refreshes_invalid_count() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let spec = std::env::var("PYTHIA_TEST_DB").unwrap();
    let (cred, db) = spec.rsplit_once('@').unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
    let drops = ["drop package icnt_p", "drop table icnt_t purge"];
    for sql in drops {
        let _ = conn.execute(sql, &[]);
    }
    for sql in [
        "create table icnt_t (a number)",
        "create or replace package icnt_p as procedure p(a icnt_t.a%type); end;",
        "create or replace package body icnt_p as procedure p(a icnt_t.a%type) is \
         begin null; end; end;",
        "drop table icnt_t purge",
        "create table icnt_t (a number)",
    ] {
        conn.execute(sql, &[]).unwrap();
    }
    let ours = || -> u32 {
        conn.query_row_as::<u32>(
            "select count(*) from user_objects where status = 'INVALID' and object_name = 'ICNT_P'",
            &[],
        )
        .unwrap()
    };
    assert_eq!(ours(), 2, "spec and body invalid");
    let mut h = harness(true).alone();
    // Only compiles refresh the count.
    h.state_mut().test_settings_mut().invalid.refresh_minutes = 0;
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    wait_until(&mut h, "invalid count", |app| {
        app.test_invalid_total("pythia@FREEPDB1").is_some()
    });
    let total = h.state().test_invalid_total("pythia@FREEPDB1").unwrap();
    assert!(total >= 2, "{total}");
    h.state_mut().test_open(OpenRequest::Program {
        connection: "pythia@FREEPDB1".into(),
        object: ObjectRef {
            owner: "PYTHIA".into(),
            name: "ICNT_P".into(),
            object_type: pythia_db::ObjectType::Package,
        },
    });
    wait_until(&mut h, "source", |app| !app.test_focused_busy());
    let effects = h.state_mut().test_program().compile(true, false);
    h.state_mut().test_apply_focused(effects);
    wait_until(&mut h, "compile", |app| !app.test_focused_busy());
    let message = h.state_mut().test_program().message();
    assert!(matches!(&message, Some((false, _))), "{message:?}");
    assert_eq!(ours(), 0, "spec and body valid");
    // Other tests' objects come and go: the count matches the schema's.
    let counted = |h: &mut Harness<'_, PythiaApp>| {
        wait_until(h, "count refreshed", |app| {
            let now = conn
                .query_row_as::<u32>(
                    "select count(*) from user_objects where status = 'INVALID'",
                    &[],
                )
                .unwrap();
            app.test_invalid_total("pythia@FREEPDB1") == Some(now)
        });
    };
    counted(&mut h);

    // A block in a SQL window compiles the spec: one more than before.
    for sql in ["drop table icnt_t purge", "create table icnt_t (a number)"] {
        conn.execute(sql, &[]).unwrap();
    }
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin\n  execute immediate 'alter package icnt_p compile specification';\nend;\n/\n"
            .into(),
        run: true,
    });
    wait_until(&mut h, "block", |app| !app.test_focused_busy());
    assert_eq!(ours(), 1, "only the body invalid");
    counted(&mut h);

    // A change while a count runs: the app takes that count (from before
    // the change) only on its next frame, and counts again after it.
    h.state_mut().test_refresh_invalid("pythia@FREEPDB1");
    std::thread::sleep(Duration::from_secs(1));
    conn.execute("alter package icnt_p compile body", &[])
        .unwrap();
    assert_eq!(ours(), 0, "body valid");
    h.state_mut().test_refresh_invalid("pythia@FREEPDB1");
    counted(&mut h);
    for sql in drops {
        conn.execute(sql, &[]).unwrap();
    }
}

/// Formatting while typing and on paste, and learning the formatter
/// settings from the focused editor.
#[test]
fn format_automatically_and_learn_the_style() {
    let mut h = headless_harness();
    h.state_mut()
        .test_settings_mut()
        .format_auto
        .on_statement_end = true;
    h.state_mut().test_settings_mut().format_auto.on_paste = true;
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: String::new(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut().test_sql_window().editor.goto(1, 1);
    h.run_steps(2);
    let text = |h: &mut Ui| h.state_mut().test_sql_window().editor.text().to_owned();

    // Typing the `;` that ends a statement formats it.
    h.event(egui::Event::Text("select a,b   from t".into()));
    h.run_steps(2);
    assert_eq!(text(&mut h), "select a,b   from t");
    h.event(egui::Event::Text(";".into()));
    h.run_steps(2);
    assert_eq!(text(&mut h), "select a, b from t;");
    let caret = h.state_mut().test_sql_window().editor.cursor();
    assert_eq!(caret, text(&mut h).len(), "the caret stays after the ;");

    // A pasted statement is formatted where it lands.
    h.key_press(egui::Key::Enter);
    h.run_steps(1);
    h.event(egui::Event::Paste("SELECT  x,y FROM z;".into()));
    h.run_steps(2);
    assert_eq!(text(&mut h), "select a, b from t;\nselect x, y from z;");

    // Learning: the editor is written in UPPER CASE with 4 spaces.
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "BEGIN\n    IF x > 1 THEN\n        y := 1;\n    END IF;\n    FOR i IN 1..3 LOOP\n        \
               NULL;\n    END LOOP;\nEND;\n/\nSELECT a, b FROM t WHERE c = 1;\n"
            .into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut().test_settings(true);
    h.run_steps(2);
    // The pointer has never been over the dialog: hover first.
    h.get_by_label("Focused editor").hover();
    h.run_steps(1);
    h.get_by_label("Focused editor").click();
    h.run_steps(3);
    assert!(
        h.query_by_label_contains("Keywords: lower → UPPER")
            .is_some()
    );
    assert!(h.query_by_label_contains("Indent: 2 → 4").is_some());
    h.get_by_label("Apply").click();
    h.run_steps(2);
    let format = h.state_mut().test_settings_mut().format.clone();
    assert_eq!(format.keyword_case, pythia_plsql::Case::Upper);
    assert_eq!(format.indent, 4);

    // Saved as a profile.
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, "New profile")
        .focus();
    h.run_steps(1);
    h.event(egui::Event::Text("team".into()));
    h.run_steps(1);
    h.get_by_label("Save as profile").click();
    h.run_steps(2);
    let settings = h.state_mut().test_settings_mut().clone();
    assert_eq!(settings.format_profile.as_deref(), Some("team"));
    assert_eq!(settings.format_profiles.get("team"), Some(&format));
}

/// The schema's stored source teaches the formatter its style.
#[test]
fn learn_the_style_from_a_schema() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_settings(true);
    h.run_steps(2);
    h.get_by_label("Current schema").hover();
    h.run_steps(1);
    h.get_by_label("Current schema").click();
    h.run_steps(1);
    assert!(h.state().test_learning());
    wait_until(&mut h, "learning", |app| !app.test_learning());
    assert!(
        h.query_by_label_contains("Learned from schema PYTHIA")
            .is_some(),
        "{:?}",
        h.query_by_label_contains("Nothing to learn")
            .map(|n| format!("{n:?}"))
    );
}

/// Statements the formatter keeps as written go to the AI with the rules;
/// an answer is applied only if it changes nothing but the layout.
#[test]
fn format_with_ai() {
    use std::sync::{Arc, Mutex};
    let mut h = headless_harness();
    let requests: Arc<Mutex<Vec<String>>> = Default::default();
    let seen = requests.clone();
    h.state_mut()
        .test_ai(Arc::new(move |request: &pythia_ai::ChatRequest| {
            let sent = request.messages.last().unwrap().content.clone();
            seen.lock().unwrap().push(sent.clone());
            Ok(if sent.contains("t_a") {
                "```sql\nCREATE OR REPLACE TRIGGER t_a\n  FOR UPDATE ON t\n  COMPOUND TRIGGER\n  \
                 BEFORE STATEMENT IS\n  BEGIN\n    NULL;\n  END BEFORE STATEMENT;\nEND t_a;\n```"
                    .into()
            } else {
                // Not just layout: a comment was added.
                "```sql\ncreate or replace trigger t_b for update on t compound trigger\n\
                 -- fires once\nbefore statement is begin null; end before statement;\nend t_b;\n```"
                    .into()
            })
        }));
    let t_b = "create or replace trigger t_b for update on t compound trigger\n\
               before statement is begin null; end before statement;\nend t_b;";
    let text = format!(
        "SELECT 1 FROM dual;\n\
         create or replace trigger t_a for update on t compound trigger\n\
         before statement is begin null; end before statement;\nend t_a;\n/\n{t_b}\n/\n"
    );
    h.state_mut().test_sql_window().editor.set_text(&text);
    h.run_steps(2);

    h.state_mut()
        .test_command(crate::commands::Command::AiFormat);
    wait_until(&mut h, "the answers", |app| {
        app.test_log()
            .iter()
            .any(|l| l.starts_with("Format with AI: laid out"))
    });
    let log = h.state().test_log().last().unwrap().clone();
    assert_eq!(
        log,
        "Format with AI: laid out 1 of 2 statement(s); kept as written: \
         line 11: the answer changed more than the layout"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].contains("- Indent by 2 spaces per level"),
        "{}",
        requests[0]
    );
    assert_eq!(
        h.state_mut().test_sql_window().editor.text(),
        format!(
            "select 1 from dual;\nCREATE OR REPLACE TRIGGER t_a\n  FOR UPDATE ON t\n  COMPOUND TRIGGER\n  \
             BEFORE STATEMENT IS\n  BEGIN\n    NULL;\n  END BEFORE STATEMENT;\nEND t_a;\n/\n{t_b}\n/\n"
        )
    );
    // One undo step back to what the rules made of it.
    h.state_mut().test_sql_window().editor.inner().undo();
    h.run_steps(1);
    assert!(
        h.state_mut()
            .test_sql_window()
            .editor
            .text()
            .starts_with("select 1 from dual;\ncreate or replace trigger t_a")
    );
}

/// Commit and Rollback follow the focused window's transaction, and never
/// open a session just to end nothing.
#[test]
fn toolbar_follows_the_transaction() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin\n  execute immediate 'drop table pythia_toolbar_t';\nexception\n  when others then null;\nend;\n/\n\
               create table pythia_toolbar_t (n number);\n\
               insert into pythia_toolbar_t values (1);\n"
            .into(),
        run: false,
    });
    h.run_steps(2);
    assert_eq!(
        h.state().test_transaction_state(),
        Err("No transaction in this window")
    );
    h.state_mut()
        .test_command(crate::commands::Command::Rollback);
    h.state_mut().test_command(crate::commands::Command::Commit);
    h.run_steps(2);
    assert!(!h.state().test_has_doc_session(), "no session opened");
    h.state_mut().test_execute_focused();
    wait_until(&mut h, "pending transaction", |app| {
        app.test_transaction_state().is_ok()
    });
    h.state_mut()
        .test_command(crate::commands::Command::Rollback);
    wait_until(&mut h, "rolled back", |app| {
        app.test_transaction_state() == Err("No transaction")
    });
}

/// Auto-commit is a setting: SQL windows follow it, also those already
/// open, and production connections never auto-commit.
#[test]
fn autocommit_setting() {
    use crate::commands::Command;
    let Some((profile, password)) = profile() else {
        return;
    };
    let script = "begin\n  execute immediate 'drop table pythia_autocommit_t';\nexception\n  when others then null;\nend;\n/\n\
                  create table pythia_autocommit_t (n number);\n\
                  insert into pythia_autocommit_t values (1);\n";
    let open = |h: &mut Ui| {
        h.state_mut().test_open(OpenRequest::Sql {
            connection: "pythia@FREEPDB1".into(),
            text: script.into(),
            run: false,
        });
        h.run_steps(2);
    };
    let run = |h: &mut Ui| {
        h.state_mut().test_execute_focused();
        h.run_steps(2);
        wait_until(h, "script", |app| !app.test_focused_busy());
    };

    let mut h = harness(true);
    h.state_mut().test_settings_mut().autocommit = true;
    h.state_mut()
        .test_connect(profile.clone(), password.clone());
    wait_until(&mut h, "connection", |app| app.test_connected());
    open(&mut h);
    run(&mut h);
    assert_eq!(h.state().test_transaction_state(), Err("Auto-commit is on"));
    // Another session sees the row: it was committed.
    let spec = std::env::var("PYTHIA_TEST_DB").unwrap();
    let (cred, db) = spec.rsplit_once('@').unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let other = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
    let rows = || {
        other
            .query_row_as::<u32>("select count(*) from pythia_autocommit_t", &[])
            .unwrap()
    };
    assert_eq!(rows(), 1);
    // Switched off, the open window's session follows.
    h.state_mut().test_command(Command::ToggleAutocommit);
    assert!(!h.state_mut().test_settings_mut().autocommit);
    run(&mut h);
    assert_eq!(h.state().test_transaction_state(), Ok(()));
    assert_eq!(rows(), 0, "the new row isn't committed");
    h.state_mut().test_command(Command::Rollback);
    wait_until(&mut h, "rolled back", |app| {
        app.test_transaction_state() == Err("No transaction")
    });
    drop(h);

    let mut h = harness(true);
    h.state_mut().test_settings_mut().autocommit = true;
    let production = ConnectionProfile {
        production: true,
        ..profile
    };
    h.state_mut().test_connect(production, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    open(&mut h);
    h.state_mut().test_execute_focused();
    h.run_steps(2);
    h.get_by_label("Run on production").click();
    h.run_steps(2);
    wait_until(&mut h, "script", |app| !app.test_focused_busy());
    assert_eq!(h.state().test_transaction_state(), Ok(()));
    h.state_mut().test_command(Command::Rollback);
    wait_until(&mut h, "rolled back", |app| {
        app.test_transaction_state() == Err("No transaction")
    });
}

/// The result toolbar fits narrow SQL windows: its buttons, the filter and
/// the row count never overlap, and buttons drop their labels when needed.
#[test]
fn result_toolbar_fits_narrow_windows() {
    use egui::accesskit::Role;
    use egui_kittest::kittest::By;
    let Some((profile, password)) = profile() else {
        return;
    };
    for width in [1440.0, 760.0, 600.0, 480.0] {
        let mut h = sized_harness(egui::vec2(width, 700.0));
        h.state_mut()
            .test_connect(profile.clone(), password.clone());
        wait_until(&mut h, "connection", |app| app.test_connected());
        h.state_mut().test_open(OpenRequest::Sql {
            connection: "pythia@FREEPDB1".into(),
            text: "select * from demo_employees".into(),
            run: true,
        });
        wait_until(&mut h, "query", |app| !app.test_focused_busy());
        // The buttons may drop their labels, which takes a frame.
        h.run_steps(4);
        save(&mut h, &format!("result-toolbar-{width}"));

        // The toolbar is the row below the result's tab.
        let tab = h.get_by_label("Result 1").rect();
        let in_row = |r: &egui::Rect| {
            let y = r.center().y;
            y > tab.max.y && y < tab.max.y + 40.0
        };
        let filter = h
            .query_all(By::new().role(Role::TextInput))
            .map(|n| n.rect())
            .find(in_row)
            .expect("filter field");
        let mut items = vec![("filter", filter)];
        match h.query_by_label_contains("columns ·") {
            Some(info) => items.push(("row count", info.rect())),
            None => assert!(width < 600.0, "{width}: row count"),
        }
        for node in h.query_all(By::new().role(Role::Button)) {
            let r = node.rect();
            if in_row(&r) && r.min.x >= filter.min.x {
                items.push(("button", r));
            }
        }
        assert!(items.len() >= 6, "{width}: {items:?}");
        // Below that the window is too narrow for more than not overlapping.
        if width >= 600.0 {
            assert!(filter.width() >= 80.0, "{width}: filter {filter:?}");
        }
        for (i, (a, ra)) in items.iter().enumerate() {
            assert!(ra.max.x <= width, "{width}: {a} outside the window: {ra:?}");
            for (b, rb) in &items[i + 1..] {
                assert!(
                    !ra.shrink(0.5).intersects(rb.shrink(0.5)),
                    "{width}: {a} {ra:?} overlaps {b} {rb:?}"
                );
            }
        }
        if width >= 1440.0 {
            assert!(h.query_by_label("Copy all").is_some(), "{width}: labels");
        }
    }
}

/// One connection for the whole app (#80): connecting makes the new
/// connection the active one, and the object browser, new SQL windows and
/// the open SQL windows follow it; windows with a transaction are asked
/// about first. A program window keeps its connection, which its tab
/// names. Disconnecting the active connection hands over to another.
#[test]
fn one_active_connection() {
    use crate::commands::Command;
    let Some((profile, password)) = profile() else {
        return;
    };
    let first = "pythia@FREEPDB1";
    let other = ConnectionProfile {
        name: "other".into(),
        color: None,
        ..profile.clone()
    };
    let spec = std::env::var("PYTHIA_TEST_DB").unwrap();
    let (cred, db) = spec.rsplit_once('@').unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
    let _ = conn.execute("drop table pythia_active_t purge", &[]);
    conn.execute("create table pythia_active_t (n number)", &[])
        .unwrap();
    let rows = || -> i64 {
        conn.query_row_as("select count(*) from pythia_active_t", &[])
            .unwrap()
    };

    let mut h = harness(true);
    h.state_mut().test_connect(profile, password.clone());
    wait_until(&mut h, "first connection", |app| {
        app.test_connected_to(first)
    });
    h.state_mut().test_command(Command::NewSqlWindow);
    h.run_steps(2);
    h.state_mut().test_open(OpenRequest::Program {
        connection: first.into(),
        object: ObjectRef {
            owner: "PYTHIA".into(),
            name: "DEMO_PKG".into(),
            object_type: pythia_db::ObjectType::Package,
        },
    });
    wait_until(&mut h, "package", |app| !app.test_focused_busy());
    let on = |h: &Ui, prefix: &str| -> Vec<Option<String>> {
        h.state()
            .test_window_connections()
            .into_iter()
            .filter(|(title, _)| title.starts_with(prefix))
            .map(|(_, c)| c)
            .collect()
    };
    let some = |c: &str| Some(c.to_owned());
    // All SQL windows on `connection`: how many.
    let sql_on = |h: &Ui, connection: &str| -> usize {
        let windows = on(h, "SQL");
        assert!(
            windows.iter().all(|c| c.as_deref() == Some(connection)),
            "SQL windows on {connection}: {windows:?}"
        );
        windows.len()
    };

    // Connecting: the new connection is the active one; the SQL window
    // moves, the package stays and its tab says where it is.
    h.state_mut().test_connect(other, password);
    wait_until(&mut h, "second connection", |app| {
        app.test_connected_to("other")
    });
    h.run_steps(2);
    assert_eq!(
        h.state().test_active_connection(),
        (some("other"), some("other"))
    );
    let windows = sql_on(&h, "other");
    assert_eq!(on(&h, "demo_pkg"), [some(first)]);
    assert_eq!(
        h.state().test_tab_title("demo_pkg"),
        Some(format!("demo_pkg · {first}")),
        "the package's tab names its connection"
    );
    // New windows open on it, whichever window is focused.
    h.state_mut().test_command(Command::NewSqlWindow);
    h.run_steps(2);
    assert_eq!(sql_on(&h, "other"), windows + 1);

    // A transaction: switching (the object browser's picker) asks first.
    h.state_mut()
        .test_sql_window()
        .editor
        .set_text("insert into pythia_active_t values (1)");
    h.state_mut().test_execute_focused();
    wait_until(&mut h, "pending transaction", |app| {
        app.test_transaction_state().is_ok()
    });
    h.state_mut().test_open(OpenRequest::Browse {
        connection: first.into(),
    });
    h.run_steps(2);
    assert_eq!(h.state().test_active_connection().0, some("other"));
    h.get_by_label("Roll back and switch").click();
    h.run_steps(4);
    assert_eq!(
        h.state().test_active_connection(),
        (some(first), some(first))
    );
    assert_eq!(sql_on(&h, first), windows + 1);
    wait_until(&mut h, "rolled back", |_| rows() == 0);
    assert_eq!(
        h.state().test_tab_title("demo_pkg").as_deref(),
        Some("demo_pkg"),
        "the package is on the active connection again"
    );

    // Disconnecting the active connection: the other one takes over.
    h.state_mut().test_command(Command::Disconnect);
    h.run_steps(2);
    assert_eq!(
        h.state().test_active_connection(),
        (some("other"), some("other"))
    );
    assert_eq!(sql_on(&h, "other"), windows + 1);
    conn.execute("drop table pythia_active_t purge", &[])
        .unwrap();
}

/// SQL windows stay on their connection unless the settings allow
/// switching, and never switch with a pending transaction.
#[test]
fn window_connection_switching() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let other = ConnectionProfile {
        name: "other".into(),
        color: None,
        ..profile.clone()
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password.clone());
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_connect(other, password);
    wait_until(&mut h, "second connection", |app| {
        app.test_connected_to("other")
    });
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin\n  execute immediate 'drop table pythia_switch_t';\nexception\n  when others then null;\nend;\n/\n\
               create table pythia_switch_t (n number);\n\
               insert into pythia_switch_t values (1);\n"
            .into(),
        run: false,
    });
    h.run_steps(2);
    let on = |h: &Ui| h.state().test_focused_connection().unwrap();

    // Off by default: no picker, and the window stays.
    assert!(h.query_by_label("Connection").is_none());
    h.state_mut().test_rebind_focused("other");
    assert_eq!(on(&h), "pythia@FREEPDB1");

    h.state_mut().test_settings_mut().switch_window_connection = true;
    h.run_steps(2);
    assert!(h.query_by_label("Connection").is_some());
    save(&mut h, "window-connection");
    h.state_mut().test_execute_focused();
    wait_until(&mut h, "pending transaction", |app| {
        app.test_transaction_state().is_ok()
    });
    h.state_mut().test_rebind_focused("other");
    assert_eq!(on(&h), "pythia@FREEPDB1", "not with a pending transaction");
    h.state_mut()
        .test_command(crate::commands::Command::Rollback);
    wait_until(&mut h, "rolled back", |app| {
        app.test_transaction_state() == Err("No transaction")
    });
    h.state_mut().test_rebind_focused("other");
    assert_eq!(on(&h), "other");

    // Switching off again: the window goes back to the connection the
    // object browser shows, the app's, and moves when it is disconnected.
    h.state_mut().test_settings_mut().switch_window_connection = false;
    h.run_steps(2);
    assert_eq!(on(&h), "pythia@FREEPDB1");
    assert!(h.query_by_label("Connection").is_none());
    h.state_mut()
        .test_command(crate::commands::Command::Disconnect);
    h.run_steps(2);
    assert_eq!(on(&h), "other");
    assert!(h.query_by_label("Connection").is_none());
}

/// After disconnecting one of two connections, Execute in a window that
/// was on it runs on the other one instead of opening the Connect dialog
/// (#84).
#[test]
fn execute_after_disconnecting_its_connection() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let other = ConnectionProfile {
        name: "other".into(),
        color: None,
        ..profile.clone()
    };
    let mut h = harness(true);
    h.state_mut().test_connect(other, password.clone());
    wait_until(&mut h, "first connection", |app| {
        app.test_connected_to("other")
    });
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "second connection", |app| {
        app.test_connected_to("pythia@FREEPDB1")
    });
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "select 42 as answer from dual".into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::Disconnect);
    h.run_steps(2);
    assert!(!h.state().test_connected_to("pythia@FREEPDB1"));
    h.state_mut().test_execute_focused();
    h.run_steps(2);
    assert!(!h.state().test_connect_dialog_open(), "no Connect dialog");
    assert_eq!(
        h.state().test_focused_connection().as_deref(),
        Some("other")
    );
    wait_until(&mut h, "the query", |app| !app.test_focused_busy());
    let window = h.state_mut().test_sql_window();
    let errors: Vec<&str> = window
        .state
        .messages
        .iter()
        .filter(|m| m.level == crate::sql_window::Level::Error)
        .map(|m| m.text.as_str())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(window.state.results.len(), 1, "the query ran");
}

/// The toolbar's Debug button is enabled only with the cursor in an
/// anonymous block of a connected SQL window.
#[test]
fn toolbar_debug_follows_the_cursor() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let text = "select 1 from dual;\n\nbegin\n  dbms_output.put_line('hi');\nend;\n/\n";
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: text.into(),
        run: false,
    });
    let cursor_at = |h: &mut Ui, needle: &str| {
        let i = text.find(needle).unwrap();
        h.state_mut().test_sql_window().editor.select(i..i);
        h.run_steps(2);
        h.state().test_debug_state()
    };
    assert_eq!(
        cursor_at(&mut h, "dual"),
        Err("Debugging runs an anonymous block (BEGIN … END;): put the cursor in one")
    );
    assert_eq!(cursor_at(&mut h, "put_line"), Ok(()));
    h.state_mut().test_program_with(
        ObjectRef {
            owner: "PYTHIA".into(),
            name: "P".into(),
            object_type: pythia_db::ObjectType::Procedure,
        },
        "create or replace procedure p as begin null; end;",
        "",
    );
    h.run_steps(2);
    assert!(
        h.state()
            .test_debug_state()
            .is_err_and(|r| r.starts_with("Debugging starts from a SQL window"))
    );
}

/// The assistant panel with a fake model: a statement is explained, its
/// bind values stay back until allowed in the settings, and code from the
/// answer replaces the statement.
#[test]
fn ai_assistant_panel() {
    use std::sync::{Arc, Mutex};
    let mut h = harness(true);
    let requests: Arc<Mutex<Vec<String>>> = Default::default();
    let seen = requests.clone();
    h.state_mut()
        .test_ai(Arc::new(move |request: &pythia_ai::ChatRequest| {
            let mut seen = seen.lock().unwrap();
            let sent: Vec<&str> = request.messages.iter().map(|m| m.content.as_str()).collect();
            seen.push(format!("{}\n{}", request.system, sent.join("\n")));
            Ok(if seen.len() == 1 {
                "It reads the name of one employee by number.\n\n```sql\nselect ename\n  from emp\n where empno = 7839;\n```\n\
                 - The **primary key** makes this a `UNIQUE SCAN`.\n- A literal shows the plan for one value."
                    .into()
            } else {
                "With :id = 7839 it returns one row.".into()
            })
        }));
    let text = "-- names\nselect ename from emp where empno = :id;\n";
    let w = h.state_mut().test_sql_window();
    w.editor.set_text(text);
    let at = text.find("select").unwrap();
    w.editor.select(at..at);
    w.bind_values.insert("id".into(), Some("7839".into()));
    h.run_steps(2);

    h.state_mut()
        .test_command(crate::commands::Command::AiExplain);
    wait_until(&mut h, "the answer", |app| app.test_ai_answers() == 1);
    assert!(
        h.query_all_by_label_contains("where empno = 7839")
            .next()
            .is_some()
    );
    assert!(
        h.query_by_label_contains("Not sent: Bind variable values")
            .is_some()
    );
    {
        let seen = requests.lock().unwrap();
        assert!(seen[0].contains("select ename from emp where empno = :id"));
        assert!(!seen[0].contains("7839"), "bind value sent: {}", seen[0]);
    }
    save(&mut h, "18-ai-assistant-dark");

    // Allowed now: the follow-up's request carries the value. Ctrl+Enter in
    // the input sends it rather than executing the statement.
    h.state_mut().test_settings_mut().ai.send_bind_values = true;
    h.state_mut().test_assistant().focus(crate::ai::Mode::Ask);
    h.run_steps(2);
    h.event(egui::Event::Text("Which row does it return?".into()));
    h.run_steps(1);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Enter);
    wait_until(&mut h, "the second answer", |app| {
        app.test_ai_answers() == 2
    });
    assert!(!h.state().test_connect_dialog_open(), "the statement ran");
    let follow_up = requests.lock().unwrap()[1].clone();
    assert!(
        follow_up.ends_with("Which row does it return?"),
        "{follow_up}"
    );
    assert!(follow_up.contains(":id = '7839'"), "{follow_up}");

    // Code from the first answer replaces the statement; the editor keeps
    // its terminator.
    h.get_by_label("Replace").click();
    h.run_steps(2);
    assert_eq!(
        h.state_mut().test_sql_window().editor.text(),
        "-- names\nselect ename\n  from emp\n where empno = 7839;\n"
    );
}

/// With a connection, requests carry the definitions of the tables used, a
/// plan, and tables found for a description.
#[test]
fn ai_with_database() {
    use std::sync::{Arc, Mutex};
    let Some((profile, password)) = profile() else {
        return;
    };
    let spec = std::env::var("PYTHIA_TEST_DB").unwrap();
    let (cred, db) = spec.rsplit_once('@').unwrap();
    let (user, pw) = cred.split_once('/').unwrap();
    let conn = pythia_db::oracle::Connection::connect(user, pw, db).unwrap();
    let _ = conn.execute("drop table pythia_ai_invoices purge", &[]);
    conn.execute(
        "create table pythia_ai_invoices (invoice_id number primary key, amount number(12,2), issued date)",
        &[],
    )
    .unwrap();
    let mut h = headless_harness();
    let requests: Arc<Mutex<Vec<String>>> = Default::default();
    let seen = requests.clone();
    h.state_mut()
        .test_ai(Arc::new(move |request: &pythia_ai::ChatRequest| {
            let last = request.messages.last().unwrap().content.clone();
            seen.lock().unwrap().push(last);
            Ok("```sql\nselect 1 from dual\n```".into())
        }));
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "select * from pythia_ai_invoices where amount > 100;\n".into(),
        run: false,
    });
    h.run_steps(2);
    h.state_mut()
        .test_command(crate::commands::Command::AiAnalyzePlan);
    wait_until(&mut h, "the plan analysis", |app| {
        app.test_ai_answers() == 1
    });
    {
        let sent = &requests.lock().unwrap()[0];
        assert!(sent.contains("## Database\nOracle"), "{sent}");
        assert!(
            sent.contains("## TABLE PYTHIA.PYTHIA_AI_INVOICES\n"),
            "{sent}"
        );
        assert!(sent.contains("Primary key (INVOICE_ID)"), "{sent}");
        assert!(sent.contains("## Execution plan (EXPLAIN PLAN)"), "{sent}");
        assert!(sent.contains("TABLE ACCESS FULL"), "{sent}");
    }

    h.state_mut().test_assistant().mode = crate::ai::Mode::Generate;
    h.state_mut().test_assistant().input = "invoices above 1000 from last month".into();
    h.run_steps(1);
    h.get_by_label("Send").click();
    wait_until(&mut h, "the generated query", |app| {
        app.test_ai_answers() == 2
    });
    {
        let sent = &requests.lock().unwrap()[1];
        assert!(sent.contains("PYTHIA_AI_INVOICES"), "{sent}");
        assert!(
            sent.contains("invoices above 1000 from last month"),
            "{sent}"
        );
    }
    conn.execute("drop table pythia_ai_invoices purge", &[])
        .unwrap();
}

/// Settings → AI with a configured backend.
#[test]
fn settings_ai() {
    if out_dir().is_none() {
        return;
    }
    let mut h = harness(true);
    h.state_mut()
        .test_ai(std::sync::Arc::new(|_: &pythia_ai::ChatRequest| {
            Ok("OK".into())
        }));
    let claude = pythia_config::AiBackend::new(pythia_config::AiKind::Anthropic, "shot".into());
    h.state_mut()
        .test_settings_mut()
        .ai
        .backends
        .insert(0, claude);
    h.state_mut().test_settings_ai();
    h.run_steps(3);
    save(&mut h, "settings-ai");
}

/// A MySQL connection through an SSH server (`PYTHIA_TEST_SSH` and
/// `PYTHIA_TEST_MYSQL_VIA_SSH`, see pythia-db's MySQL tests): the connect
/// dialog asks to trust the server's host key, then connects, and SQL
/// windows get sessions through the same tunnel.
#[test]
fn mysql_through_ssh() {
    use crate::connections::Passwords;
    let (Ok(ssh), Ok(spec)) = (
        std::env::var("PYTHIA_TEST_SSH"),
        std::env::var("PYTHIA_TEST_MYSQL_VIA_SSH"),
    ) else {
        return;
    };
    let (ssh_credentials, ssh_host) = ssh.rsplit_once('@').unwrap();
    let (ssh_user, ssh_password) = ssh_credentials.split_once(':').unwrap();
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (username, password) = credentials.split_once('/').unwrap();
    let profile = ConnectionProfile {
        name: "shop over ssh".into(),
        kind: pythia_config::DbKind::MySql,
        username: username.into(),
        connect_string: connect_string.into(),
        ssh: pythia_config::SshTunnel {
            enabled: true,
            host: ssh_host.into(),
            username: ssh_user.into(),
            key_file: String::new(),
        },
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("pythia-app-ssh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let known_hosts = dir.join("known_hosts");

    let mut h = harness(true);
    h.state_mut().test_connect_dialog(
        profile,
        Passwords {
            database: password.into(),
            ssh: ssh_password.into(),
        },
        known_hosts.clone(),
    );
    wait_until(&mut h, "the host key question", |app| {
        app.test_connect_dialog_error()
            .is_some_and(|e| e.contains("isn't known yet"))
    });
    save(&mut h, "mysql-ssh-host-key");
    h.get_by_label("Trust and connect").click();
    wait_until(&mut h, "connection", |app| {
        app.test_connected_to("shop over ssh")
    });
    assert!(!h.state().test_connect_dialog_open());
    let saved = std::fs::read_to_string(&known_hosts).unwrap();
    assert!(saved.contains(" ssh-"), "{saved}");
    let log = h.state().test_log();
    assert!(
        log.iter()
            .any(|l| l == &format!("Connecting to shop over ssh through {ssh_host}…")),
        "{log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l.starts_with("Trusted the SSH host key of ")),
        "{log:?}"
    );

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "shop over ssh".into(),
        text: "select database() as db, 6 * 7 as answer;".into(),
        run: true,
    });
    wait_until(&mut h, "query", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    let result = w.state.results.last().expect("a result");
    assert_eq!(
        result.rows,
        [vec![Some("pythia".into()), Some("42".into())]]
    );
    save(&mut h, "mysql-ssh-sql-window");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Nothing shows through behind the dock (#64): the light theme had black
/// in the tabs' rounded corners and between the dock's areas. The test
/// renderer clears to transparent, so such spots are transparent pixels.
#[test]
fn nothing_shows_through_behind_the_dock() {
    // Needs a GPU adapter, like the screenshots.
    if out_dir().is_none() {
        return;
    }
    for dark in [false, true] {
        // The start layout: browser and project on the left, a SQL window
        // on the right.
        let mut h = locked(dark, true);
        h.run_steps(4);
        let image = h.render().expect("render");
        let holes: Vec<(u32, u32)> = image
            .enumerate_pixels()
            .filter(|(_, _, p)| p.0[3] < 255)
            .map(|(x, y, _)| (x, y))
            .collect();
        assert!(
            holes.is_empty(),
            "dark {dark}: {} transparent pixels, e.g. {:?}",
            holes.len(),
            &holes[..holes.len().min(5)]
        );
        save(&mut h, if dark { "dock-dark" } else { "dock-light" });
    }
}

/// Every Settings page fits Pythia's default window: the close button stays
/// on screen however tall the page is (#62), and closes the dialog. No page
/// makes the window wider, also with a bigger interface font: a hint on one
/// line pushed the page list off the screen (#113).
#[test]
fn settings_pages_fit_the_window() {
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
    for font_size in [13.0, 18.0] {
        let mut h = sized_harness(screen.size());
        h.state_mut().test_settings_mut().appearance.font_size = font_size;
        let ctx = h.ctx.clone();
        h.state_mut().test_apply_settings(&ctx);
        let mut page = 0;
        let mut first_close = None;
        while h.state_mut().test_settings_page(page) {
            h.run_steps(4);
            let at = format!("{font_size} pt, page {page}");
            let close = h.get_by_label("Close window").rect();
            assert!(
                screen.contains_rect(close),
                "{at}: close button at {close:?}"
            );
            let first_close = *first_close.get_or_insert(close);
            assert_eq!(close, first_close, "{at}: the window changed size");
            for entry in h.query_all_by_label("Appearance") {
                let rect = entry.rect();
                assert!(screen.contains_rect(rect), "{at}: Appearance at {rect:?}");
            }
            page += 1;
        }
        assert!(page > 5, "{page} pages");
        h.state_mut().test_settings(true);
        h.run_steps(4);
        save(
            &mut h,
            &format!("settings-formatter-small-window-{font_size}pt"),
        );
        h.get_by_label("Close window").click();
        h.run_steps(2);
        assert!(!h.state().test_settings_open());
    }
}

/// The New profile field lines up with the other rows' controls when a
/// bigger interface font widens the label column (#116).
#[test]
fn settings_formatter_rows_line_up() {
    let mut h = sized_harness(egui::vec2(1280.0, 800.0));
    h.state_mut().test_settings_mut().appearance.font_size = 18.0;
    let ctx = h.ctx.clone();
    h.state_mut().test_apply_settings(&ctx);
    h.state_mut().test_settings(true);
    h.run_steps(4);
    let field = h.get_by_label("New profile").rect();
    let learn = h.get_by_label("Focused editor").rect();
    assert_eq!(field.min.x, learn.min.x, "{field:?} {learn:?}");
}

/// The MySQL dashboard (`PYTHIA_TEST_MYSQL`): server KPIs, trends and
/// findings on the connection's own session, and what the user would need
/// to see more.
#[test]
fn mysql_dashboard() {
    use crate::commands::Command;
    let Some(spec) = std::env::var("PYTHIA_TEST_MYSQL").ok() else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (username, password) = credentials.split_once('/').unwrap();
    let profile = ConnectionProfile {
        name: "shop".into(),
        kind: pythia_config::DbKind::MySql,
        username: username.into(),
        connect_string: connect_string.into(),
        color: Some([0x14, 0xb8, 0xa6]),
        ..Default::default()
    };
    // Tall enough for all of it.
    let mut h = sized_harness(egui::vec2(1440.0, 2100.0));
    h.state_mut().test_connect(profile, password.into());
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_command(Command::MonitorDashboard);

    // Three snapshots: rates from the second on, a chart from the third.
    let wait_for = |h: &mut Ui, what: &str, done: &dyn Fn(&mut Ui) -> bool| {
        let start = Instant::now();
        while !done(h) {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "timed out waiting for {what}"
            );
            h.step();
            std::thread::sleep(Duration::from_millis(15));
        }
    };
    wait_for(&mut h, "the dashboard", &|h| {
        h.query_by_label("Buffer pool hit ratio").is_some()
    });
    // Refreshing now until the charts have two samples.
    let start = Instant::now();
    while h.query_all_by_label("Collecting samples…").next().is_some() {
        assert!(start.elapsed() < Duration::from_secs(60), "no charts");
        std::thread::sleep(Duration::from_millis(600));
        h.get_by_label("⟳").click();
        h.run_steps(3);
    }
    for label in [
        "Connections",
        "Queries / s",
        "Buffer pool used",
        "Temp tables on disk",
        "Largest tables",
        "Running statements, longest first",
    ] {
        assert!(h.query_all_by_label(label).next().is_some(), "{label}");
    }
    assert_eq!(h.state().test_focused_title().as_deref(), Some("Dashboard"));
    save(&mut h, "mysql-dashboard");
    sessions(&mut h, "mysql");
}

/// Monitor: Sessions on a MySQL, PostgreSQL or SQL Server connection
/// (#131): every session, idle ones too, in a tab of its own; a session
/// opens its details. Screenshots are named after `tag`.
fn sessions(h: &mut Harness<'_, PythiaApp>, tag: &str) {
    use crate::commands::Command;
    h.state_mut().test_command(Command::MonitorSessions);
    // The header counts sessions before the list is in: wait for the
    // table (#137).
    let start = Instant::now();
    while h.query_by_label("Statement").is_none() {
        assert!(
            h.query_by_label_contains("Sessions are not available")
                .is_none(),
            "the list loads"
        );
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out waiting for the sessions"
        );
        h.step();
        std::thread::sleep(Duration::from_millis(15));
    }
    h.run_steps(3);
    assert_eq!(h.state().test_focused_title().as_deref(), Some("Sessions"));
    assert!(h.query_by_label_contains(" sessions · ").is_some());
    assert!(h.query_by_label("Blocking tree").is_some());
    save(h, &format!("{tag}-sessions"));
}

/// The PostgreSQL dashboard (`PYTHIA_TEST_POSTGRES`, #92): server KPIs,
/// trends, findings, sessions, storage and top statements.
#[test]
fn postgres_dashboard() {
    use crate::commands::Command;
    let Some(spec) = std::env::var("PYTHIA_TEST_POSTGRES").ok() else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (username, password) = credentials.split_once('/').unwrap();
    let profile = ConnectionProfile {
        name: "orders".into(),
        kind: pythia_config::DbKind::Postgres,
        username: username.into(),
        connect_string: connect_string.into(),
        color: Some([0x14, 0xb8, 0xa6]),
        ..Default::default()
    };
    // Tall enough for all of it.
    let mut h = sized_harness(egui::vec2(1440.0, 2100.0));
    h.state_mut().test_connect(profile, password.into());
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_command(Command::MonitorDashboard);

    // Three snapshots: rates from the second on, a chart from the third.
    let wait_for = |h: &mut Ui, what: &str, done: &dyn Fn(&mut Ui) -> bool| {
        let start = Instant::now();
        while !done(h) {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "timed out waiting for {what}"
            );
            h.step();
            std::thread::sleep(Duration::from_millis(15));
        }
    };
    wait_for(&mut h, "the dashboard", &|h| {
        h.query_by_label("Cache hit ratio").is_some()
    });
    // Refreshing now until the charts have two samples.
    let start = Instant::now();
    while h.query_all_by_label("Collecting samples…").next().is_some() {
        assert!(start.elapsed() < Duration::from_secs(60), "no charts");
        std::thread::sleep(Duration::from_millis(600));
        h.get_by_label("⟳").click();
        h.run_steps(3);
    }
    for label in [
        "Connections",
        "Commits / s",
        "Transaction ID age",
        "Largest databases",
        "Running statements, longest first",
        "Top statements by total time (since the statistics were reset)",
    ] {
        assert!(h.query_all_by_label(label).next().is_some(), "{label}");
    }
    assert_eq!(h.state().test_focused_title().as_deref(), Some("Dashboard"));
    save(&mut h, "postgres-dashboard");
    sessions(&mut h, "postgres");
}

/// A MySQL connection without a database (#81): the toolbar, status bar
/// and object browser say so; picking one in the browser makes it the
/// database of the open SQL windows and of new ones.
#[test]
fn mysql_database_from_the_browser() {
    let Some(spec) = std::env::var("PYTHIA_TEST_MYSQL").ok() else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (username, password) = credentials.split_once('/').unwrap();
    let (server, database) = connect_string.rsplit_once('/').unwrap();
    let profile = ConnectionProfile {
        name: "nodb@mysql".into(),
        kind: pythia_config::DbKind::MySql,
        username: username.into(),
        connect_string: server.into(),
        ..Default::default()
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile.clone(), password.into());
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.run_steps(2);
    assert!(
        h.state()
            .test_log()
            .iter()
            .any(|l| l.starts_with("Connected to nodb@mysql (MySQL ")
                && l.ends_with(", no database)")),
        "{:?}",
        h.state().test_log()
    );
    // The toolbar and the status bar.
    let shown = |h: &Ui, label: &str| h.query_all_by_label_contains(label).count();
    assert_eq!(shown(&h, "nodb@mysql · No database"), 2);
    assert!(h.query_by_label("No database selected").is_some());

    let current = |h: &mut Ui| -> Option<String> {
        h.state_mut()
            .test_sql_window()
            .editor
            .set_text("select database() as db");
        h.state_mut().test_execute_focused();
        wait_until(h, "select database()", |app| !app.test_focused_busy());
        let w = h.state_mut().test_sql_window();
        let result = w.state.results.last().expect("a result");
        result.rows[0][0].clone()
    };
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "nodb@mysql".into(),
        text: String::new(),
        run: false,
    });
    h.run_steps(2);
    assert_eq!(current(&mut h), None);

    // Picked in the browser's picker, as a user does (#102): the panel's
    // effects reach the app together with the frame's others.
    h.get_by_value("No database").click();
    let start = Instant::now();
    while h.query_by_label(database).is_none() {
        assert!(start.elapsed() < Duration::from_secs(60), "the databases");
        h.step();
        std::thread::sleep(Duration::from_millis(15));
    }
    h.get_by_label(database).click();
    h.run_steps(2);
    assert_eq!(
        current(&mut h).as_deref(),
        Some(database),
        "the open window"
    );
    h.state_mut()
        .test_command(crate::commands::Command::NewSqlWindow);
    h.run_steps(2);
    assert_eq!(current(&mut h).as_deref(), Some(database), "a new window");
    assert_eq!(shown(&h, &format!("nodb@mysql · {database}")), 2);
    assert!(h.query_by_label("No database selected").is_none());

    // Connected again, it starts in the database picked last (#133).
    let picked = format!("nodb@mysql: database {database}");
    let picks = |app: &PythiaApp| app.test_log().iter().filter(|l| **l == picked).count();
    h.state_mut().test_connect(profile.clone(), password.into());
    wait_until(&mut h, "the database picked last", |app| {
        app.test_connected() && picks(app) == 2
    });
    assert_eq!(current(&mut h).as_deref(), Some(database), "again");
    assert_eq!(shown(&h, &format!("nodb@mysql · {database}")), 2);
    assert!(h.query_by_label("No database selected").is_none());

    // A database from the profile wins.
    let profile = ConnectionProfile {
        default_schema: "information_schema".into(),
        ..profile
    };
    h.state_mut().test_connect(profile, password.into());
    wait_until(&mut h, "connection", |app| {
        app.test_connected() && !app.test_browser_loading()
    });
    assert_eq!(
        current(&mut h).as_deref(),
        Some("information_schema"),
        "the profile's"
    );
    assert_eq!(picks(h.state()), 2);
    assert_eq!(shown(&h, "nodb@mysql · information_schema"), 2);
    let marks = &h.state_mut().test_browser().marks;
    assert_eq!(
        marks.databases.get("nodb@mysql").map(String::as_str),
        Some(database),
        "kept for a profile without one"
    );
}

/// A MySQL connection (`PYTHIA_TEST_MYSQL=user/password@host:port/database`):
/// queries, transactions, browser and object views work; what MySQL can't
/// do is left out of the menus, the toolbar and the browser.
#[test]
fn mysql_connection() {
    use crate::commands::Command;
    use pythia_db::ObjectType;
    let Some(spec) = std::env::var("PYTHIA_TEST_MYSQL").ok() else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (username, password) = credentials.split_once('/').unwrap();
    let schema = connect_string.rsplit_once('/').map(|(_, db)| db).unwrap();
    let profile = ConnectionProfile {
        name: "pythia@mysql".into(),
        kind: pythia_config::DbKind::MySql,
        username: username.into(),
        connect_string: connect_string.into(),
        color: Some([0x14, 0xb8, 0xa6]),
        ..Default::default()
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password.into());
    wait_until(&mut h, "connection", |app| app.test_connected());
    assert!(
        h.state()
            .test_log()
            .iter()
            .any(|l| l.starts_with("Connected to pythia@mysql (MySQL ")),
        "{:?}",
        h.state().test_log()
    );

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@mysql".into(),
        text: "drop table if exists app_orders;
create table app_orders (
  id int primary key,
  customer varchar(30) not null comment 'Who ordered',
  total decimal(8,2),
  ordered datetime default current_timestamp
);
insert into app_orders (id, customer, total) values (1, 'Ada', 12.50), (2, 'Linus', 99.00), (3, 'Grace', 250.00);
set @limit = 100;
select id, customer, total from app_orders where total < @limit order by id;
"
        .into(),
        run: true,
    });
    wait_until(&mut h, "script", |app| !app.test_focused_busy());
    let w = h.state_mut().test_sql_window();
    let result = w.state.results.last().expect("a result");
    let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "customer", "total"]);
    assert_eq!(result.rows.len(), 2, "SET ran on the server");
    assert_eq!(
        h.state().test_transaction_state(),
        Ok(()),
        "the insert waits for commit"
    );
    save(&mut h, "mysql-sql-window");
    h.state_mut().test_command(Command::Commit);
    wait_until(&mut h, "commit", |app| {
        app.test_transaction_state() == Err("No transaction")
    });

    // Oracle-only menus are gone, the rest only offers what works.
    for menu in ["Debug", "Reports"] {
        assert!(h.query_by_label(menu).is_none(), "{menu}");
    }
    h.get_by_label("Monitor").click();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Dashboard").is_some());
    assert!(h.query_by_label_contains("Blocking Sessions").is_some());
    assert!(h.query_by_label_contains("AWR").is_none());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    h.get_by_label("Tools").click();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Find Database Object").is_some());
    assert!(h.query_by_label_contains("Scheduler Jobs").is_none());
    assert!(h.query_by_label_contains("Compare Schemas").is_none());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);

    // A shortcut to something hidden explains instead of running it.
    h.state_mut().test_command(Command::MonitorAwr);
    assert_eq!(
        h.state().test_log().last().map(String::as_str),
        Some("AWR / ASH Reports isn't available for MySQL connections")
    );

    // The browser has MySQL's folders.
    h.state_mut()
        .test_expand_browser(&[ObjectType::Table, ObjectType::Procedure]);
    wait_until(&mut h, "browser", |app| !app.test_browser_loading());
    h.run_steps(3);
    save(&mut h, "mysql-browser");
    assert!(h.query_by_label("Procedures").is_some());
    assert!(h.query_by_label("Packages").is_none());
    assert!(h.query_by_label_contains("app_orders").is_some());

    // The session's own user, `user@host`, and its form; without access to
    // `mysql` it says what it can't read.
    let user = format!("{username}@%");
    h.state_mut().test_browser().set_filter(username);
    h.run_steps(2);
    user_forms(&mut h, "mysql", &user);
    h.state_mut().test_browser().set_filter("");
    h.run_steps(2);

    // Tables open on their columns; MySQL's details are tabs.
    h.state_mut().test_open(OpenRequest::Object {
        connection: "pythia@mysql".into(),
        object: ObjectRef {
            owner: schema.into(),
            name: "app_orders".into(),
            object_type: ObjectType::Table,
        },
    });
    h.run_steps(2);
    wait_until(&mut h, "object view", |app| !app.test_focused_busy());
    assert_eq!(h.state().test_object_view().loaded_for_test().1, 4);
    assert!(h.query_by_label("Constraints").is_some());
    save(&mut h, "mysql-object-view");

    // Completion: the columns after an alias, from MySQL's dictionary.
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@mysql".into(),
        text: "select o\n  from app_orders o".into(),
        run: false,
    });
    h.run_steps(3);
    h.state_mut().test_sql_window().editor.select(8..8);
    h.run_steps(3);
    h.event(egui::Event::Text(".".into()));
    wait_until(&mut h, "completion popup", |app| {
        app.test_sql_window_ref()
            .is_some_and(|w| w.completion_visible_with("customer"))
    });
    save(&mut h, "mysql-completion");

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@mysql".into(),
        text: "drop table app_orders;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// What every connection to another database checks in the app: a script
/// ran (its last result has `columns` and `rows` rows) and left a change
/// pending; Oracle-only menus are gone; the browser has `folders`, and
/// lists `user` among the users; tables open on their columns; completion
/// offers a table's columns after an alias. Screenshots are named after
/// `tag`.
struct OtherDatabase<'a> {
    tag: &'a str,
    connection: &'a str,
    product: &'a str,
    schema: &'a str,
    /// The connection's user, as the browser lists it.
    user: &'a str,
    folders: &'a [pythia_db::ObjectType],
}

impl OtherDatabase<'_> {
    fn check(&self, h: &mut Harness<'_, PythiaApp>, script: &str, columns: &[&str], rows: usize) {
        use crate::commands::Command;
        use pythia_db::ObjectType;
        let connected = format!("Connected to {} ({} ", self.connection, self.product);
        assert!(
            h.state()
                .test_log()
                .iter()
                .any(|l| l.starts_with(&connected)),
            "{:?}",
            h.state().test_log()
        );

        h.state_mut().test_open(OpenRequest::Sql {
            connection: self.connection.into(),
            text: script.into(),
            run: true,
        });
        wait_until(h, "script", |app| !app.test_focused_busy());
        let w = h.state_mut().test_sql_window();
        let errors: Vec<&str> = w
            .state
            .messages
            .iter()
            .filter(|m| m.level == crate::sql_window::Level::Error)
            .map(|m| m.text.as_str())
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        let result = w.state.results.last().expect("a result");
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, columns);
        assert_eq!(result.rows.len(), rows);
        assert_eq!(
            h.state().test_transaction_state(),
            Ok(()),
            "the insert waits for commit"
        );
        save(h, &format!("{}-sql-window", self.tag));
        h.state_mut().test_command(Command::Commit);
        wait_until(h, "commit", |app| {
            app.test_transaction_state() == Err("No transaction")
        });

        // Oracle-only menus are gone; a shortcut to one explains. Monitor
        // has the dashboard (#92) and sessions (#131).
        for menu in ["Debug", "Reports"] {
            assert!(h.query_by_label(menu).is_none(), "{menu}");
        }
        assert!(h.query_by_label("Monitor").is_some());
        h.state_mut().test_command(Command::MonitorAwr);
        assert_eq!(
            h.state().test_log().last().cloned(),
            Some(format!(
                "AWR / ASH Reports isn't available for {} connections",
                self.product
            ))
        );

        h.state_mut()
            .test_expand_browser(&[ObjectType::Table, ObjectType::Procedure]);
        wait_until(h, "browser", |app| !app.test_browser_loading());
        h.run_steps(3);
        save(h, &format!("{}-browser", self.tag));
        for folder in self.folders {
            assert!(h.query_by_label(folder.plural()).is_some(), "{folder:?}");
        }
        assert!(h.query_by_label("Packages").is_none());
        assert!(h.query_by_label_contains("app_orders").is_some());

        // Users and roles below the folders (#130), with their forms
        // (#142): New user… on Users, Edit user… on one.
        assert!(h.query_by_label("Roles").is_some());
        assert!(h.query_by_label("Profiles").is_none());
        h.state_mut().test_browser().set_filter(self.user);
        h.run_steps(2);
        user_forms(h, self.tag, self.user);
        assert!(h.query_by_label("Copy name").is_none());
        h.state_mut().test_browser().set_filter("");
        h.run_steps(2);

        h.state_mut().test_open(OpenRequest::Object {
            connection: self.connection.into(),
            object: ObjectRef {
                owner: self.schema.into(),
                name: "app_orders".into(),
                object_type: ObjectType::Table,
            },
        });
        h.run_steps(2);
        wait_until(h, "object view", |app| !app.test_focused_busy());
        assert_eq!(h.state().test_object_view().loaded_for_test().1, 4);
        assert!(h.query_by_label("Constraints").is_some());
        save(h, &format!("{}-object-view", self.tag));

        h.state_mut().test_open(OpenRequest::Sql {
            connection: self.connection.into(),
            text: "select o\n  from app_orders o".into(),
            run: false,
        });
        h.run_steps(3);
        h.state_mut().test_sql_window().editor.select(8..8);
        h.run_steps(3);
        h.event(egui::Event::Text(".".into()));
        wait_until(h, "completion popup", |app| {
            app.test_sql_window_ref()
                .is_some_and(|w| w.completion_visible_with("customer"))
        });
        save(h, &format!("{}-completion", self.tag));
    }
}

/// The user forms of a database other than Oracle (#142), with the browser
/// filtered to `user`: New user… on Users opens a new user's form, the
/// user's menu has what works on every database and Edit user… opens its
/// form, read from the database.
fn user_forms(h: &mut Harness<'_, PythiaApp>, tag: &str, user: &str) {
    h.get_by_label("Users").click_secondary();
    h.run_steps(2);
    h.get_by_label("New user…").click();
    h.run_steps(2);
    wait_until(h, "new user form", |app| !app.test_focused_busy());
    assert_eq!(h.state().test_focused_title().as_deref(), Some("New user"));
    assert!(h.query_by_label("Privileges").is_some(), "the form loaded");
    save(h, &format!("{tag}-new-user"));

    h.get_by_label("Users").click();
    h.run_steps(2);
    wait_until(h, "users", |app| !app.test_browser_loading());
    h.run_steps(2);
    h.get_by_label(user).click_secondary();
    h.run_steps(2);
    save(h, &format!("{tag}-users"));
    assert!(h.query_by_label("Copy name").is_some());
    assert!(h.query_by_label("Drop…").is_some());
    h.get_by_label("Edit user…").click();
    h.run_steps(2);
    wait_until(h, "user form", |app| !app.test_focused_busy());
    h.run_steps(2);
    assert_eq!(
        h.state().test_focused_title(),
        Some(format!("{user} (user)"))
    );
    assert!(
        h.query_by_label(&format!("User {user}")).is_some(),
        "the user was read"
    );
    save(h, &format!("{tag}-edit-user"));
}

/// Connects the harness to the database of `env`
/// (`user/password@connect string`) as `name`.
fn connect_to(env: &str, name: &str, kind: pythia_config::DbKind) -> Option<Ui> {
    let spec = std::env::var(env).ok()?;
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (username, password) = credentials.split_once('/').unwrap();
    let profile = ConnectionProfile {
        name: name.into(),
        kind,
        username: username.into(),
        connect_string: connect_string.into(),
        color: Some([0x3b, 0x82, 0xf6]),
        ..Default::default()
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password.into());
    wait_until(&mut h, "connection", |app| app.test_connected());
    Some(h)
}

/// A PostgreSQL connection (`PYTHIA_TEST_POSTGRES=user/password@host:port/database`):
/// a script with a dollar-quoted function and casts, transactions, browser,
/// object views and completion.
#[test]
fn postgres_connection() {
    use pythia_db::ObjectType;
    let Some(mut h) = connect_to(
        "PYTHIA_TEST_POSTGRES",
        "pythia@postgres",
        pythia_config::DbKind::Postgres,
    ) else {
        return;
    };
    let database = OtherDatabase {
        tag: "postgres",
        connection: "pythia@postgres",
        product: "PostgreSQL",
        schema: "public",
        user: "pythia",
        folders: &[
            ObjectType::Table,
            ObjectType::MaterializedView,
            ObjectType::Function,
            ObjectType::Sequence,
        ],
    };
    database.check(
        &mut h,
        "drop table if exists app_orders cascade;
create table app_orders (
  id int primary key,
  customer varchar(30) not null,
  total numeric(8,2),
  ordered timestamp default now()
);
comment on column app_orders.customer is 'Who ordered';
create or replace function app_below(p_limit numeric) returns numeric language plpgsql as $$
begin
  -- A body with statements of its own.
  return (select sum(total) from app_orders where total < p_limit);
end;
$$;
insert into app_orders (id, customer, total) values (1, 'Ada', 12.50), (2, 'Linus', 99.00), (3, 'Grace', 250.00);
set work_mem = '8MB';
select id, customer, total::text, app_below(100) as below
  from app_orders where total < 100::numeric order by id;
",
        &["id", "customer", "total", "below"],
        2,
    );
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@postgres".into(),
        text: "drop function app_below;\ndrop table app_orders;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// A SQL Server connection (`PYTHIA_TEST_MSSQL=user/password@host:port/database?trust`):
/// a script of `GO` batches with a procedure and a variable, transactions,
/// browser, object views and completion.
#[test]
fn sqlserver_connection() {
    use pythia_db::ObjectType;
    let Some(mut h) = connect_to(
        "PYTHIA_TEST_MSSQL",
        "pythia@sqlserver",
        pythia_config::DbKind::SqlServer,
    ) else {
        return;
    };
    let database = OtherDatabase {
        tag: "sqlserver",
        connection: "pythia@sqlserver",
        product: "SQL Server",
        schema: "dbo",
        user: "pythia",
        folders: &[
            ObjectType::Table,
            ObjectType::Procedure,
            ObjectType::Sequence,
            ObjectType::Synonym,
        ],
    };
    database.check(
        &mut h,
        "drop table if exists app_orders;
create table app_orders (
  id int primary key,
  customer varchar(30) not null,
  total decimal(8,2),
  ordered datetime2 default sysdatetime()
);
GO
create or alter procedure app_below @limit decimal(8,2) as
begin
  set nocount on;
  select id, customer, total from app_orders where total < @limit order by id;
end
GO
insert into app_orders (id, customer, total) values (1, 'Ada', 12.50), (2, 'Linus', 99.00), (3, 'Grace', 250.00);
declare @limit decimal(8,2) = 100;
select id, customer, total, @limit as limit from app_orders where total < @limit order by id;
",
        &["id", "customer", "total", "limit"],
        2,
    );
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@sqlserver".into(),
        text: "drop procedure app_below;\ndrop table app_orders;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// The SQL Server dashboard (`PYTHIA_TEST_MSSQL_DBA`, a login with VIEW
/// SERVER STATE): server KPIs, trends, waits and storage on the
/// connection's own session.
#[test]
fn sqlserver_dashboard() {
    use crate::commands::Command;
    let Some(spec) = std::env::var("PYTHIA_TEST_MSSQL_DBA").ok() else {
        return;
    };
    let (credentials, connect_string) = spec.rsplit_once('@').unwrap();
    let (username, password) = credentials.split_once('/').unwrap();
    let profile = ConnectionProfile {
        name: "warehouse".into(),
        kind: pythia_config::DbKind::SqlServer,
        username: username.into(),
        connect_string: connect_string.into(),
        color: Some([0xe1, 0x1d, 0x48]),
        ..Default::default()
    };
    // Tall enough for all of it.
    let mut h = sized_harness(egui::vec2(1440.0, 2200.0));
    h.state_mut().test_connect(profile, password.into());
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_command(Command::MonitorDashboard);

    let start = Instant::now();
    while h.query_by_label("Page life expectancy").is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out waiting for the dashboard"
        );
        h.step();
        std::thread::sleep(Duration::from_millis(15));
    }
    // Refreshing now until the charts have two samples.
    let start = Instant::now();
    while h.query_all_by_label("Collecting samples…").next().is_some() {
        assert!(start.elapsed() < Duration::from_secs(60), "no charts");
        std::thread::sleep(Duration::from_millis(600));
        h.get_by_label("⟳").click();
        h.run_steps(3);
    }
    for label in [
        "Connections",
        "Batch requests / s",
        "Buffer cache hit ratio",
        "Top waits (since the refresh before)",
        "Largest databases",
        "Running requests, longest first",
    ] {
        assert!(h.query_all_by_label(label).next().is_some(), "{label}");
    }
    assert_eq!(h.state().test_focused_title().as_deref(), Some("Dashboard"));
    save(&mut h, "sqlserver-dashboard");
    sessions(&mut h, "sqlserver");
}

/// An error raised in a package called from a trigger: one click opens the
/// code where it was raised, and each stack line opens its own (#112).
#[test]
fn error_stack() {
    use crate::sql_window::FrameTarget;
    use pythia_db::ObjectType;
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(false);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "create table pythia_es_orders (id number, qty number);
create or replace package pythia_es_stock as
  procedure reserve(p_qty number);
end;
/
create or replace package body pythia_es_stock as
  procedure check_qty(p_qty number) is
    l_ok number;
  begin
    select 1 into l_ok from dual where p_qty <= 3;
  end;

  procedure reserve(p_qty number) is
  begin
    check_qty(p_qty);
  end;
end;
/
create or replace trigger pythia_es_orders_trg
before insert on pythia_es_orders
for each row
begin
  pythia_es_stock.reserve(:new.qty);
end;
/"
        .into(),
        run: true,
    });
    wait_until(&mut h, "setup", |app| !app.test_focused_busy());
    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "begin\n  insert into pythia_es_orders values (1, 5);\nend;".into(),
        run: true,
    });
    let sql = h.state().test_focused();
    wait_until(&mut h, "error stack", |app| {
        app.test_sql_window_ref().is_some_and(|w| {
            w.state.messages.iter().any(|m| {
                !m.stack.is_empty()
                    && !m
                        .stack
                        .iter()
                        .any(|f| matches!(f.target, FrameTarget::Pending(_)))
            })
        })
    });
    let w = h.state_mut().test_sql_window();
    let message = w
        .state
        .messages
        .iter()
        .find(|m| !m.stack.is_empty())
        .unwrap();
    let opens = message.stack.iter().filter(|f| f.target.opens()).count();
    assert_eq!(
        opens, 4,
        "package twice, trigger, statement: {}",
        message.text
    );
    h.get_by_label("Open where raised").hover();
    save(&mut h, "error_stack");

    h.get_by_label("Open where raised").click();
    wait_until(&mut h, "package body at the line", |app| {
        app.test_focused_title()
            .is_some_and(|t| t == "pythia_es_stock")
    });
    wait_for_source(&mut h);
    let program = h.state_mut().test_program();
    let part = &program.parts[program.active];
    assert_eq!(part.object_type, ObjectType::PackageBody);
    assert_eq!(part.editor.line_col().0, 5, "select 1 into l_ok ...");
    save(&mut h, "error_stack_open");

    // The trigger's line counts from its block: line 2 there is line 5.
    h.state_mut().test_focus(sql);
    h.run_steps(3);
    h.get_by_label_contains("PYTHIA_ES_ORDERS_TRG\", line")
        .click();
    wait_until(&mut h, "trigger", |app| {
        app.test_focused_title()
            .is_some_and(|t| t == "pythia_es_orders_trg")
    });
    wait_for_source(&mut h);
    let program = h.state_mut().test_program();
    let part = &program.parts[program.active];
    assert_eq!(part.object_type, ObjectType::Trigger);
    assert_eq!(part.editor.line_col().0, 5, "{}", part.editor.text());

    h.state_mut().test_open(OpenRequest::Sql {
        connection: "pythia@FREEPDB1".into(),
        text: "drop table pythia_es_orders purge;\ndrop package pythia_es_stock;".into(),
        run: true,
    });
    wait_until(&mut h, "cleanup", |app| !app.test_focused_busy());
}

/// Steps the app until the focused program window shows its source.
fn wait_for_source(harness: &mut Harness<'_, PythiaApp>) {
    let start = Instant::now();
    loop {
        let editor = harness.state_mut().test_program_editor();
        if editor.is_some_and(|e| !e.text().is_empty()) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out waiting for the source"
        );
        harness.step();
        std::thread::sleep(Duration::from_millis(15));
    }
    harness.run_steps(3);
}

/// Runs `task` in the focused maintenance window (after `options`) and
/// waits until it is done and the state is read again: (outcome, whether
/// it went well, each statement's messages).
fn run_maintenance(
    h: &mut Harness<'_, PythiaApp>,
    dialect: pythia_db::Dialect,
    task: crate::maintenance::Task,
    options: impl FnOnce(&mut crate::maintenance::Options),
) -> (String, bool, Vec<Vec<String>>) {
    let window = h.state_mut().test_maintenance_window();
    window.task = Some(task);
    options(&mut window.options);
    let effects = window.run_for_test(dialect);
    h.state_mut().test_apply(effects);
    h.run_steps(2);
    wait_until(h, task.label(dialect), |app| !app.test_focused_busy());
    let (outcome, messages) = h.state_mut().test_maintenance_window().outcome_for_test();
    let (text, ok) = outcome.expect("an outcome");
    (text, ok, messages)
}

/// A line of the focused maintenance window's state: (value, warning).
fn maintenance_fact(h: &mut Harness<'_, PythiaApp>, label: &str) -> Option<(String, bool)> {
    let facts = h.state_mut().test_maintenance_window().facts_for_test()?;
    let fact = facts.state.iter().find(|f| f.label == label)?;
    Some((fact.value.clone(), fact.warn))
}

/// Opens the maintenance window of `object` and waits for its state.
fn open_maintenance(h: &mut Harness<'_, PythiaApp>, connection: &str, object: ObjectRef) {
    h.state_mut().test_open(OpenRequest::Maintenance {
        connection: connection.into(),
        object,
        task: None,
    });
    h.run_steps(2);
    wait_until(h, "maintenance state", |app| !app.test_focused_busy());
}

/// Runs a script in a new SQL window and closes it.
fn run_script(h: &mut Harness<'_, PythiaApp>, connection: &str, sql: &str) {
    h.state_mut().test_open(OpenRequest::Sql {
        connection: connection.into(),
        text: sql.into(),
        run: true,
    });
    wait_until(h, sql, |app| !app.test_focused_busy());
    h.state_mut().test_close_focused();
}

/// Maintenance of an Oracle table and index (#144): an offline move leaves
/// the indexes unusable, which the window shows and rebuilds; statistics
/// are gathered, locked (gathering then needs force) and unlocked; the
/// table shrinks and moves online; the index rebuilds.
#[test]
fn maintenance_oracle() {
    use crate::maintenance::Task;
    use pythia_db::{Dialect, ObjectType};
    let Some((profile, password)) = profile() else {
        return;
    };
    let owner = profile.username.to_uppercase();
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let connection = "pythia@FREEPDB1";
    run_script(
        &mut h,
        connection,
        "begin
  for t in (select table_name from user_tables where table_name = 'PYTHIA_MT_ORDERS') loop
    execute immediate 'drop table ' || t.table_name || ' purge';
  end loop;
end;
/
create table pythia_mt_orders (id number primary key, note varchar2(100));
create index pythia_mt_orders_ix on pythia_mt_orders (note);
insert into pythia_mt_orders select level, 'order ' || level from dual connect by level <= 2000;
commit;
alter table pythia_mt_orders move;",
    );
    let table = ObjectRef {
        owner: owner.clone(),
        name: "PYTHIA_MT_ORDERS".into(),
        object_type: ObjectType::Table,
    };
    open_maintenance(&mut h, connection, table);
    assert_eq!(
        h.state().test_focused_title().as_deref(),
        Some("pythia_mt_orders maintenance")
    );
    let window = h.state().test_focused();
    assert_eq!(
        maintenance_fact(&mut h, "Indexes"),
        Some(("2, 2 indexes unusable".into(), true))
    );
    assert_eq!(
        maintenance_fact(&mut h, "Statistics gathered"),
        Some(("never".into(), true))
    );

    // The unusable indexes rebuild, online (Free has it).
    h.state_mut().test_maintenance_window().task = Some(Task::RebuildUnusable);
    let statements = h
        .state_mut()
        .test_maintenance_window()
        .statements_for_test(Dialect::ORACLE)
        .unwrap();
    assert_eq!(statements.len(), 2, "{statements:?}");
    assert!(statements.contains(&format!(
        "alter index {owner}.PYTHIA_MT_ORDERS_IX rebuild online"
    )));
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::RebuildUnusable, |_| {});
    assert!(ok, "{text}");
    assert_eq!(
        maintenance_fact(&mut h, "Indexes"),
        Some(("2".into(), false))
    );

    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::GatherStats, |_| {});
    assert!(ok, "{text}");
    assert_eq!(
        maintenance_fact(&mut h, "Rows (statistics)"),
        Some(("2,000".into(), false))
    );
    let (gathered, warn) = maintenance_fact(&mut h, "Statistics gathered").unwrap();
    assert!(gathered.ends_with(" ago") && !warn, "{gathered}");

    // Locked statistics are only gathered with force.
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::LockStats, |_| {});
    assert!(ok, "{text}");
    assert_eq!(
        maintenance_fact(&mut h, "Statistics locked"),
        Some(("yes".into(), true))
    );
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::GatherStats, |o| {
        o.force = false;
    });
    assert!(!ok && text.contains("ORA-20005"), "{text}");
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::GatherStats, |o| {
        o.force = true;
    });
    assert!(ok, "{text}");
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::UnlockStats, |_| {});
    assert!(ok, "{text}");
    assert_eq!(maintenance_fact(&mut h, "Statistics locked"), None);

    // Half the rows go; the table shrinks (with row movement for as long
    // as it takes) and moves online, keeping its indexes usable.
    run_script(
        &mut h,
        connection,
        "delete from pythia_mt_orders where mod(id, 2) = 0;\ncommit;",
    );
    h.state_mut().test_focus(window);
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::Shrink, |_| {});
    assert!(ok, "{text}");
    assert_eq!(
        maintenance_fact(&mut h, "Row movement"),
        Some(("disabled".into(), false))
    );
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::Move, |o| {
        o.online = true;
    });
    assert!(ok, "{text}");
    assert_eq!(
        maintenance_fact(&mut h, "Indexes"),
        Some(("2".into(), false))
    );
    h.state_mut().test_maintenance_window().task = Some(Task::GatherStats);
    h.run_steps(2);
    save(&mut h, "maintenance");

    // The index on its own.
    let index = ObjectRef {
        owner: owner.clone(),
        name: "PYTHIA_MT_ORDERS_IX".into(),
        object_type: ObjectType::Index,
    };
    open_maintenance(&mut h, connection, index);
    assert_eq!(
        maintenance_fact(&mut h, "Status"),
        Some(("valid".into(), false))
    );
    h.state_mut().test_maintenance_window().task = Some(Task::RebuildIndex);
    assert_eq!(
        h.state_mut()
            .test_maintenance_window()
            .statements_for_test(Dialect::ORACLE)
            .unwrap(),
        [format!(
            "alter index {owner}.PYTHIA_MT_ORDERS_IX rebuild online"
        )]
    );
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::RebuildIndex, |_| {});
    assert!(ok, "{text}");
    let (text, ok, _) = run_maintenance(&mut h, Dialect::ORACLE, Task::Shrink, |_| {});
    assert!(ok, "{text}");

    run_script(&mut h, connection, "drop table pythia_mt_orders purge;");
}

/// Maintenance of a PostgreSQL table (#144): VACUUM (with VERBOSE's
/// messages), ANALYZE and REINDEX CONCURRENTLY, which refuses a
/// transaction block: the window's session autocommits.
#[test]
fn maintenance_postgres() {
    use crate::maintenance::Task;
    use pythia_db::{Dialect, ObjectType};
    let connection = "pythia@postgres";
    let Some(mut h) = connect_to(
        "PYTHIA_TEST_POSTGRES",
        connection,
        pythia_config::DbKind::Postgres,
    ) else {
        return;
    };
    run_script(
        &mut h,
        connection,
        "drop table if exists pythia_mt_orders;
create table pythia_mt_orders (id int primary key, note text);
create index pythia_mt_orders_note on pythia_mt_orders (note);
insert into pythia_mt_orders select g, 'order ' || g from generate_series(1, 5000) g;
delete from pythia_mt_orders where id % 2 = 0;
commit;",
    );
    let table = ObjectRef {
        owner: "public".into(),
        name: "pythia_mt_orders".into(),
        object_type: ObjectType::Table,
    };
    open_maintenance(&mut h, connection, table);
    assert_eq!(
        maintenance_fact(&mut h, "Indexes"),
        Some(("2".into(), false))
    );
    let (text, ok, messages) = run_maintenance(&mut h, Dialect::POSTGRES, Task::Vacuum, |o| {
        o.analyze = true;
        o.verbose = true;
    });
    assert!(ok, "{text}");
    assert!(
        messages[0].iter().any(|m| m.contains("pythia_mt_orders")),
        "{messages:?}"
    );
    let (vacuumed, _) = maintenance_fact(&mut h, "Last vacuum").unwrap();
    assert!(vacuumed.contains(" ago"), "{vacuumed}");
    let (text, ok, _) = run_maintenance(&mut h, Dialect::POSTGRES, Task::Reindex, |o| {
        o.concurrently = true;
    });
    assert!(ok, "{text}");
    let (text, ok, _) = run_maintenance(&mut h, Dialect::POSTGRES, Task::Analyze, |o| {
        o.verbose = false;
    });
    assert!(ok, "{text}");
    let (text, ok, _) = run_maintenance(&mut h, Dialect::POSTGRES, Task::Vacuum, |o| {
        o.full = true;
        o.verbose = false;
    });
    assert!(ok, "{text}");
    run_script(&mut h, connection, "drop table pythia_mt_orders;");
}

/// Maintenance of a MySQL table (#144): ANALYZE, OPTIMIZE and CHECK TABLE
/// answer with rows, shown as messages.
#[test]
fn maintenance_mysql() {
    use crate::maintenance::Task;
    use pythia_db::{Dialect, ObjectType};
    let connection = "pythia@mysql";
    let Some(spec) = std::env::var("PYTHIA_TEST_MYSQL").ok() else {
        return;
    };
    let database = spec.rsplit('/').next().unwrap_or_default().to_owned();
    let Some(mut h) = connect_to(
        "PYTHIA_TEST_MYSQL",
        connection,
        pythia_config::DbKind::MySql,
    ) else {
        return;
    };
    run_script(
        &mut h,
        connection,
        "drop table if exists pythia_mt_orders;
create table pythia_mt_orders (id int primary key, note varchar(100), key (note));
insert into pythia_mt_orders values (1, 'one'), (2, 'two'), (3, 'three');
commit;",
    );
    let table = ObjectRef {
        owner: database,
        name: "pythia_mt_orders".into(),
        object_type: ObjectType::Table,
    };
    open_maintenance(&mut h, connection, table);
    assert_eq!(
        maintenance_fact(&mut h, "Engine"),
        Some(("InnoDB".into(), false))
    );
    for task in [Task::Analyze, Task::Optimize, Task::Check] {
        let (text, ok, messages) = run_maintenance(&mut h, Dialect::MYSQL, task, |_| {});
        assert!(ok, "{task:?}: {text}");
        assert!(
            messages[0].iter().any(|m| m == "status: OK"),
            "{task:?}: {messages:?}"
        );
    }
    run_script(&mut h, connection, "drop table pythia_mt_orders;");
}

/// Maintenance of a SQL Server table (#144): statistics with a full scan,
/// rebuilding and reorganizing its indexes.
#[test]
fn maintenance_sqlserver() {
    use crate::maintenance::Task;
    use pythia_db::{Dialect, ObjectType};
    let connection = "pythia@sqlserver";
    let Some(mut h) = connect_to(
        "PYTHIA_TEST_MSSQL",
        connection,
        pythia_config::DbKind::SqlServer,
    ) else {
        return;
    };
    run_script(
        &mut h,
        connection,
        "drop table if exists pythia_mt_orders;
create table pythia_mt_orders (id int primary key, note varchar(100));
create index pythia_mt_orders_note on pythia_mt_orders (note);
insert into pythia_mt_orders
select top 5000 row_number() over (order by (select null)), 'order'
  from sys.all_objects a cross join sys.all_objects b;
commit;",
    );
    let table = ObjectRef {
        owner: "dbo".into(),
        name: "pythia_mt_orders".into(),
        object_type: ObjectType::Table,
    };
    open_maintenance(&mut h, connection, table);
    assert_eq!(
        maintenance_fact(&mut h, "Rows"),
        Some(("5,000".into(), false))
    );
    assert_eq!(
        maintenance_fact(&mut h, "Indexes"),
        Some(("2".into(), false))
    );
    for task in [
        Task::UpdateStatistics,
        Task::RebuildIndexes,
        Task::ReorganizeIndexes,
    ] {
        let (text, ok, _) = run_maintenance(&mut h, Dialect::SQLSERVER, task, |o| {
            o.fullscan = true;
        });
        assert!(ok, "{task:?}: {text}");
    }
    // Each statement committed: nothing waits in the window's session.
    assert!(!h.state().test_pending_tx());
    run_script(&mut h, connection, "drop table pythia_mt_orders;");
}

/// Opens the maintenance scan of the harness's connection (Tools menu) and
/// waits for it; with `limits`, scans again with those.
fn open_scan(h: &mut Harness<'_, PythiaApp>, limits: Option<crate::maintenance::Limits>) {
    h.state_mut()
        .test_command(crate::commands::Command::MaintenanceScan);
    h.run_steps(2);
    wait_until(h, "maintenance scan", |app| !app.test_focused_busy());
    if let Some(limits) = limits {
        let window = h.state_mut().test_scan_window();
        window.set_limits_for_test(limits);
        let effects = window.load();
        h.state_mut().test_apply(effects);
        h.run_steps(2);
        wait_until(h, "maintenance scan", |app| !app.test_focused_busy());
    }
    h.run_steps(2);
}

/// The focused scan's findings whose name starts with `prefix`, as
/// (name, problem, picked).
fn scan_findings(h: &mut Harness<'_, PythiaApp>, prefix: &str) -> Vec<(String, String, bool)> {
    let scan = h
        .state_mut()
        .test_scan_window()
        .scan_for_test()
        .expect("a scan");
    scan.findings
        .iter()
        .filter(|f| f.name.starts_with(prefix))
        .map(|f| (f.name.clone(), f.problem.clone(), f.picked))
        .collect()
}

/// Whether the focused scan has a note with `label`.
fn scan_note(h: &mut Harness<'_, PythiaApp>, label: &str) -> Option<String> {
    let scan = h.state_mut().test_scan_window().scan_for_test()?;
    let note = scan.notes.iter().find(|n| n.label == label)?;
    Some(note.value.clone())
}

/// Runs the findings whose name starts with `prefix` (and `pick` accepts)
/// as a batch; waits for it and the scan after. How it ended, and each
/// statement's object and status.
fn run_scan(
    h: &mut Harness<'_, PythiaApp>,
    prefix: &str,
    pick: impl Fn(&crate::maintenance::Finding) -> bool,
) -> (String, bool, Vec<(usize, &'static str)>) {
    let window = h.state_mut().test_scan_window();
    window.pick_for_test(|f| f.name.starts_with(prefix) && pick(f));
    let effects = window.run_for_test();
    h.state_mut().test_apply(effects);
    h.run_steps(2);
    wait_until(h, "maintenance batch", |app| !app.test_focused_busy());
    h.run_steps(2);
    let (outcome, statuses) = h.state_mut().test_scan_window().outcome_for_test();
    let (text, ok) = outcome.expect("an outcome");
    (text, ok, statuses)
}

/// The maintenance scan of an Oracle schema (#145): a table without
/// statistics, its index left unusable by a move, and a table whose
/// statistics are locked (found, but not picked: gathering takes force).
/// The batch rebuilds the index first, then gathers; the scan after finds
/// neither. Without the DBA views, the automatic task can't be read.
#[test]
fn maintenance_scan_oracle() {
    let Some((profile, password)) = profile() else {
        return;
    };
    let mut h = harness(true);
    h.state_mut().test_connect(profile, password);
    wait_until(&mut h, "connection", |app| app.test_connected());
    let connection = "pythia@FREEPDB1";
    run_script(
        &mut h,
        connection,
        "begin
  for t in (select table_name from user_tables
             where table_name in ('PYTHIA_MS_T', 'PYTHIA_MS_L')) loop
    execute immediate 'drop table ' || t.table_name || ' purge';
  end loop;
end;
/
create table pythia_ms_t (id number, note varchar2(40));
insert into pythia_ms_t select level, 'row ' || level from dual connect by level <= 500;
commit;
create index pythia_ms_t_note on pythia_ms_t (note);
alter table pythia_ms_t move;
begin dbms_stats.delete_table_stats(user, 'PYTHIA_MS_T'); end;
/
create table pythia_ms_l (id number);
begin dbms_stats.lock_table_stats(user, 'PYTHIA_MS_L'); end;
/",
    );
    open_scan(&mut h, None);
    save(&mut h, "maintenance-scan-oracle");
    let found = scan_findings(&mut h, "pythia.pythia_ms_");
    let has = |name: &str, problem: &str, picked: bool| {
        found.contains(&(name.into(), problem.into(), picked))
    };
    assert!(
        has("pythia.pythia_ms_t_note", "Unusable index", true),
        "{found:?}"
    );
    assert!(
        has("pythia.pythia_ms_t", "No statistics", true),
        "{found:?}"
    );
    assert!(
        has("pythia.pythia_ms_l", "No statistics (locked)", false),
        "{found:?}"
    );
    assert!(scan_note(&mut h, "Automatic statistics").is_some());

    let (text, ok, statuses) = run_scan(&mut h, "pythia.pythia_ms_t", |_| true);
    assert!(ok, "{text}: {statuses:?}");
    assert!(text.starts_with("Maintenance: 2 done in "), "{text}");
    assert_eq!(statuses, [(0, "done"), (1, "done")]);
    let found = scan_findings(&mut h, "pythia.pythia_ms_");
    assert_eq!(
        found,
        [(
            "pythia.pythia_ms_l".into(),
            "No statistics (locked)".into(),
            false
        )]
    );
    run_script(
        &mut h,
        connection,
        "drop table pythia_ms_t purge;
drop table pythia_ms_l purge;",
    );
}

/// The maintenance scan of a PostgreSQL database (#145): a table with
/// many dead rows, and an index a failed CREATE INDEX CONCURRENTLY left
/// invalid. The batch vacuums the one and rebuilds the other concurrently
/// (on the window's own session, which autocommits); the scan after finds
/// neither.
#[test]
fn maintenance_scan_postgres() {
    use pythia_config::DbKind;
    let connection = "pythia@postgres";
    let Some(mut h) = connect_to("PYTHIA_TEST_POSTGRES", connection, DbKind::Postgres) else {
        return;
    };
    let results = crate::test_db::execute_on(
        "PYTHIA_TEST_POSTGRES",
        DbKind::Postgres,
        &[
            "drop table if exists pythia_ms_dead",
            "drop table if exists pythia_ms_inv",
            "create table pythia_ms_dead (id int primary key, note text) \
             with (autovacuum_enabled = false)",
            "insert into pythia_ms_dead select g, 'row ' || g from generate_series(1, 3000) g",
            "analyze pythia_ms_dead",
            "delete from pythia_ms_dead where id > 1000",
            "create table pythia_ms_inv (id int) with (autovacuum_enabled = false)",
            "insert into pythia_ms_inv values (1), (1)",
            "create unique index concurrently pythia_ms_inv_ux on pythia_ms_inv (id)",
            "delete from pythia_ms_inv",
            // The counters of this session reach pg_stat_all_tables now.
            "select pg_stat_force_next_flush()",
        ],
    )
    .expect("PYTHIA_TEST_POSTGRES");
    assert!(results[8].is_err(), "the duplicates fail it: {results:?}");
    assert!(
        results.iter().enumerate().all(|(i, r)| i == 8 || r.is_ok()),
        "{results:?}"
    );
    open_scan(&mut h, None);
    save(&mut h, "maintenance-scan-postgres");
    let found = scan_findings(&mut h, "public.pythia_ms_");
    assert!(
        found.contains(&("public.pythia_ms_dead".into(), "Dead rows".into(), true)),
        "{found:?}"
    );
    assert!(
        found.contains(&(
            "public.pythia_ms_inv_ux".into(),
            "Invalid index".into(),
            true
        )),
        "{found:?}"
    );

    let (text, ok, statuses) = run_scan(&mut h, "public.pythia_ms_", |_| true);
    assert!(ok, "{text}: {statuses:?}");
    let found = scan_findings(&mut h, "public.pythia_ms_");
    assert!(found.is_empty(), "{found:?}");
    crate::test_db::execute_on(
        "PYTHIA_TEST_POSTGRES",
        DbKind::Postgres,
        &["drop table pythia_ms_dead", "drop table pythia_ms_inv"],
    );
}

/// The maintenance scan of a MySQL database (#145), with the reclaimable
/// space's line lowered to every table: OPTIMIZE TABLE runs as a batch.
/// Without access to `mysql`, the statistics' age can't be read; with it
/// (`PYTHIA_TEST_MYSQL_DBA`), statistics older than the table's last change
/// by a day are found.
#[test]
fn maintenance_scan_mysql() {
    use pythia_config::DbKind;
    let connection = "pythia@mysql";
    let Some(mut h) = connect_to("PYTHIA_TEST_MYSQL", connection, DbKind::MySql) else {
        return;
    };
    run_script(
        &mut h,
        connection,
        "drop table if exists pythia_ms_t;
create table pythia_ms_t (id int primary key, note varchar(40));
insert into pythia_ms_t values (1, 'one'), (2, 'two'), (3, 'three');
commit;",
    );
    let everything = crate::maintenance::Limits {
        free_bytes: 0.0,
        free_share: -1.0,
        ..Default::default()
    };
    open_scan(&mut h, Some(everything));
    save(&mut h, "maintenance-scan-mysql");
    let found = scan_findings(&mut h, "pythia.pythia_ms_t");
    assert_eq!(
        found,
        [(
            "pythia.pythia_ms_t".into(),
            "Reclaimable space".into(),
            true
        )]
    );
    let (text, ok, statuses) = run_scan(&mut h, "pythia.pythia_ms_t", |_| true);
    assert!(ok, "{text}: {statuses:?}");
    assert_eq!(statuses, [(0, "done")]);
    if std::env::var("PYTHIA_TEST_MYSQL_DBA").is_err() {
        assert!(scan_note(&mut h, "Statistics").is_some());
    }

    // Statistics a few days older than the table's last change.
    let found = crate::test_db::run_on("PYTHIA_TEST_MYSQL_DBA", DbKind::MySql, |c| {
        let database = c.query("select database()", &[]).unwrap()[0][0]
            .clone()
            .unwrap_or_default();
        c.execute("analyze table pythia_ms_t").unwrap();
        c.execute(
            "update mysql.innodb_table_stats set last_update = now() - interval 3 day \
              where database_name = database() and table_name = 'pythia_ms_t'",
        )
        .unwrap();
        c.execute("insert into pythia_ms_t values (4, 'four')")
            .unwrap();
        let scan =
            crate::maintenance::scan(c, &database, &crate::maintenance::Limits::default()).unwrap();
        scan.findings
            .into_iter()
            .filter(|f| f.name.ends_with(".pythia_ms_t"))
            .map(|f| (f.problem, f.statements))
            .collect::<Vec<_>>()
    });
    if let Some(found) = found {
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "Statistics behind");
        assert!(found[0].1[0].starts_with("analyze table "), "{found:?}");
    }
    run_script(&mut h, connection, "drop table pythia_ms_t;");
}

/// The maintenance scan of a SQL Server database (#145): an index on
/// random GUIDs is fragmented (with the pages' line lowered for a small
/// table); the batch rebuilds it, and the scan after doesn't find it. The
/// last good DBCC CHECKDB is a note.
#[test]
fn maintenance_scan_sqlserver() {
    use pythia_config::DbKind;
    let connection = "pythia@sqlserver";
    let Some(mut h) = connect_to("PYTHIA_TEST_MSSQL", connection, DbKind::SqlServer) else {
        return;
    };
    // Row by row: one INSERT … SELECT would be sorted by the key first.
    let results = crate::test_db::execute_on(
        "PYTHIA_TEST_MSSQL",
        DbKind::SqlServer,
        &[
            "drop table if exists pythia_ms_frag",
            "create table pythia_ms_frag (
               id uniqueidentifier not null default newid()
                  constraint pythia_ms_frag_pk primary key,
               pad char(800) not null default 'x')",
            "declare @i int = 0
             begin transaction
             while @i < 3000
             begin
               insert into pythia_ms_frag default values
               set @i = @i + 1
             end
             commit",
        ],
    )
    .expect("PYTHIA_TEST_MSSQL");
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    let small = crate::maintenance::Limits {
        pages: 10,
        ..Default::default()
    };
    open_scan(&mut h, Some(small));
    save(&mut h, "maintenance-scan-sqlserver");
    let found = scan_findings(&mut h, "dbo.pythia_ms_frag");
    assert_eq!(
        found,
        [(
            "dbo.pythia_ms_frag (pythia_ms_frag_pk)".into(),
            "Fragmented index".into(),
            true
        )],
    );
    assert!(scan_note(&mut h, "Last good CHECKDB").is_some());
    let (text, ok, statuses) = run_scan(&mut h, "dbo.pythia_ms_frag", |f| {
        f.problem == "Fragmented index"
    });
    assert!(ok, "{text}: {statuses:?}");
    let found = scan_findings(&mut h, "dbo.pythia_ms_frag");
    assert!(
        found
            .iter()
            .all(|(_, problem, _)| problem != "Fragmented index"),
        "{found:?}"
    );
    run_script(&mut h, connection, "drop table pythia_ms_frag;");
}
