//! The user and role forms against the databases: each test creates a role
//! and a user through the editor, reads them back and changes them. Skipped
//! unless the database's variable is set (`user/password@host:port/db`):
//! `PYTHIA_TEST_MYSQL_DBA` (a MySQL or MariaDB account that may create
//! users; with `PYTHIA_TEST_MYSQL`, an account without access to `mysql`
//! too), `PYTHIA_TEST_POSTGRES`, `PYTHIA_TEST_MSSQL` (a `db_owner`; with
//! `PYTHIA_TEST_MSSQL_DBA`, logins as well).

use std::any::Any;
use std::sync::mpsc;
use std::time::Duration;

use pythia_db::{Client, ConnectParams, DbError, DbEvent, DbKind, DbRequest, ObjectType, Session};

use super::{Grant, Login, RoleGrant, Scope, UserDef};
use crate::documents::{Effects, ObjectRef};
use crate::object_editor::{EditTarget, ObjectEditor};

struct Live {
    session: Session,
    wake: mpsc::Receiver<()>,
    kind: DbKind,
    /// The server is MariaDB.
    mariadb: bool,
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
            kind,
            mariadb: false,
        };
        match live.next() {
            DbEvent::Connected(server) => live.mariadb = server.banner.contains("MariaDB"),
            DbEvent::ConnectFailed(e) => panic!("{env}: {e}"),
            _ => panic!("{env}: not connected"),
        }
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

    /// Runs `f` on the session's client.
    fn run<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut dyn Client) -> Result<T, DbError> + Send + 'static,
    ) -> Result<T, DbError> {
        self.session.send(DbRequest::Run {
            id: 1,
            task: Box::new(move |client| f(client).map(|v| Box::new(v) as Box<dyn Any + Send>)),
        });
        loop {
            if let DbEvent::TaskDone { result, .. } = self.next() {
                return result.map(|any| *any.downcast::<T>().expect("the task's result"));
            }
        }
    }

    fn exec(&mut self, sql: &str) {
        let statement = sql.to_owned();
        if let Err(e) = self.run(move |c| c.execute(&statement)) {
            panic!("{sql}: {e}");
        }
    }

    /// Runs a statement whose failure doesn't matter (cleaning up).
    fn try_exec(&mut self, sql: &str) {
        let sql = sql.to_owned();
        let _ = self.run(move |c| c.execute(&sql));
    }

    /// Serves an editor's requests (on this one session) until it has no
    /// more.
    fn serve(&mut self, editor: &mut ObjectEditor, effects: Effects) {
        let mut queue: Vec<DbRequest> = effects.requests.into_iter().map(|(_, r)| r).collect();
        while let Some(request) = queue.pop() {
            let id = match &request {
                DbRequest::Run { id, .. } | DbRequest::Task { id, .. } => *id,
                other => panic!("unexpected request {other:?}"),
            };
            self.session.send(request);
            let event = loop {
                match self.next() {
                    event @ DbEvent::TaskDone { id: done, .. } if done == id => break event,
                    _ => {}
                }
            };
            queue.extend(editor.on_event(event).requests.into_iter().map(|(_, r)| r));
        }
    }

    fn open(&mut self, target: EditTarget) -> ObjectEditor {
        let mut editor = ObjectEditor::new(target);
        let effects = editor.attach(self.kind.into());
        self.serve(&mut editor, effects);
        editor
    }

    /// Applies the editor's statements; returns them.
    fn apply(&mut self, editor: &mut ObjectEditor) -> Vec<String> {
        let statements = editor.statements_for_test().expect("statements");
        let effects = editor.apply_for_test();
        self.serve(editor, effects);
        match editor.outcome() {
            Some((_, true)) => statements,
            outcome => panic!("{statements:#?}: {outcome:?}"),
        }
    }
}

fn new(object_type: ObjectType) -> EditTarget {
    EditTarget::New {
        owner: String::new(),
        object_type,
        from: None,
    }
}

fn existing(name: &str, object_type: ObjectType) -> EditTarget {
    EditTarget::Edit(ObjectRef {
        owner: String::new(),
        name: name.to_owned(),
        object_type,
    })
}

/// The form as it is now.
fn form(editor: &mut ObjectEditor) -> UserDef {
    let mut out = None;
    editor.edit_for_test(|f| out = f.downcast_mut::<UserDef>().cloned());
    out.expect("a user or role form")
}

fn set_form(editor: &mut ObjectEditor, def: &UserDef) {
    let def = def.clone();
    editor.edit_for_test(move |f| *f.downcast_mut::<UserDef>().expect("form") = def);
}

/// Fills a new form in and applies it; returns the form as read back.
fn create(live: &mut Live, object_type: ObjectType, fill: impl FnOnce(&mut UserDef)) -> UserDef {
    let mut editor = live.open(new(object_type));
    let mut def = form(&mut editor);
    fill(&mut def);
    set_form(&mut editor, &def);
    live.apply(&mut editor);
    // Applying switched the editor to the new account and read it back.
    let loaded = form(&mut editor);
    assert_eq!(loaded.browser_name(), def.browser_name());
    loaded
}

/// No statement turns what was read into what was meant: it took.
fn assert_same(loaded: &UserDef, meant: &UserDef) {
    let meant = UserDef {
        password: String::new(),
        ..meant.clone()
    };
    assert_eq!(
        super::statements(Some(loaded), &meant),
        Ok(Vec::new()),
        "read back: {loaded:#?}"
    );
}

/// Changes an existing account in its form, applies it and reads it back.
fn change(
    live: &mut Live,
    name: &str,
    object_type: ObjectType,
    edit: impl FnOnce(&mut UserDef),
) -> (UserDef, UserDef) {
    let mut editor = live.open(existing(name, object_type));
    let mut def = form(&mut editor);
    edit(&mut def);
    set_form(&mut editor, &def);
    live.apply(&mut editor);
    let loaded = form(&mut editor);
    (loaded, def)
}

fn grant() -> RoleGrant {
    RoleGrant::default()
}

/// Whether `user` logs in with `password` where `env`'s account does.
fn logs_in(env: &str, kind: DbKind, user: &str, password: &str) -> bool {
    let spec = std::env::var(env).unwrap();
    let (_, connect_string) = spec.rsplit_once('@').unwrap();
    let (tx, wake) = mpsc::channel();
    let session = Session::connect(
        ConnectParams {
            kind,
            username: user.into(),
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
    loop {
        match session.try_recv() {
            Some(DbEvent::Connected(_)) => return true,
            Some(DbEvent::ConnectFailed(_)) => return false,
            Some(_) => {}
            None => {
                wake.recv_timeout(Duration::from_secs(60))
                    .expect("timed out");
            }
        }
    }
}

/// A password with a `\` is the one typed, also where the session's
/// sql_mode has NO_BACKSLASH_ESCAPES: there `'a\\b'` was `a\\b`.
#[test]
fn mysql_passwords_keep_their_backslashes() {
    let env = "PYTHIA_TEST_MYSQL_DBA";
    let Some(mut live) = Live::connect(env, DbKind::MySql) else {
        return;
    };
    let name = "pythia_acct_bs";
    live.try_exec(&format!("drop user if exists '{name}'@'%'"));
    create(&mut live, ObjectType::User, |u| {
        u.name = name.into();
        u.password = r"a\b".into();
        u.privileges.insert(
            (Scope::Database("pythia".into()), "SELECT".into()),
            Grant::Plain,
        );
    });
    assert!(logs_in(env, DbKind::MySql, name, r"a\b"));
    live.exec("set session sql_mode = concat(@@session.sql_mode, ',NO_BACKSLASH_ESCAPES')");
    change(&mut live, &format!("{name}@%"), ObjectType::User, |u| {
        u.password = r"c\d".into();
    });
    let typed = logs_in(env, DbKind::MySql, name, r"c\d");
    live.try_exec(&format!("drop user if exists '{name}'@'%'"));
    assert!(typed, "the password isn't the one typed");
}

#[test]
fn mysql_users_and_roles() {
    let Some(mut live) = Live::connect("PYTHIA_TEST_MYSQL_DBA", DbKind::MySql) else {
        return;
    };
    let cleanup = |live: &mut Live| {
        live.try_exec("drop user if exists 'pythia_acct_u'@'%'");
        live.try_exec("drop role if exists 'pythia_acct_r'");
        live.try_exec("drop table if exists pythia_acct_t");
    };
    cleanup(&mut live);
    live.exec("create table pythia_acct_t (id int)");
    let db = "pythia".to_owned();
    let table = Scope::Object(db.clone(), "pythia_acct_t".into());

    let role = create(&mut live, ObjectType::Role, |r| {
        r.name = "pythia_acct_r".into();
        r.privileges
            .insert((Scope::Database(db.clone()), "SELECT".into()), Grant::Plain);
    });
    assert!(
        role.privileges
            .contains_key(&(Scope::Database(db.clone()), "SELECT".into()))
    );

    let mut meant = None;
    let user = create(&mut live, ObjectType::User, |u| {
        u.name = "pythia_acct_u".into();
        u.password = "Pw-acct-1".into();
        u.expire = true;
        u.roles.insert("pythia_acct_r".into(), grant());
        u.privileges
            .insert((Scope::Global, "PROCESS".into()), Grant::Plain);
        u.privileges
            .insert((Scope::Database(db.clone()), "SELECT".into()), Grant::Plain);
        u.privileges.insert(
            (Scope::Database(db.clone()), "GRANT OPTION".into()),
            Grant::Plain,
        );
        u.privileges
            .insert((table.clone(), "UPDATE".into()), Grant::Plain);
        meant = Some(u.clone());
    });
    assert_same(&user, &meant.unwrap());
    assert!(user.expire && user.locked == Some(false), "{user:#?}");
    assert!(user.roles["pythia_acct_r"].default, "{user:#?}");

    let (loaded, meant) = change(&mut live, "pythia_acct_u@%", ObjectType::User, |u| {
        u.password = "Pw-acct-2".into();
        u.expire = false;
        u.locked = Some(true);
        u.roles.get_mut("pythia_acct_r").unwrap().admin = true;
        u.roles.get_mut("pythia_acct_r").unwrap().default = false;
        u.privileges
            .remove(&(Scope::Database(db.clone()), "GRANT OPTION".into()));
        u.privileges.remove(&(Scope::Global, "PROCESS".into()));
        u.privileges
            .insert((table.clone(), "INSERT".into()), Grant::Plain);
    });
    assert_same(&loaded, &meant);
    assert!(!loaded.expire && loaded.locked == Some(true), "{loaded:#?}");

    // Without access to `mysql`, the form says what it can't read and
    // changes nothing it didn't see.
    if let Some(mut limited) = Live::connect("PYTHIA_TEST_MYSQL", DbKind::MySql) {
        let mut editor = limited.open(existing("pythia_acct_u@%", ObjectType::User));
        let unseen = form(&mut editor);
        assert!(
            unseen.unread.attributes && unseen.unread.roles,
            "{unseen:#?}"
        );
        assert!(unseen.privileges.is_empty(), "{unseen:#?}");
        assert_eq!(editor.statements_for_test(), Ok(Vec::new()));
    }
    cleanup(&mut live);
}

#[test]
fn postgres_users_and_roles() {
    let Some(mut live) = Live::connect("PYTHIA_TEST_POSTGRES", DbKind::Postgres) else {
        return;
    };
    let cleanup = |live: &mut Live| {
        for role in ["pythia_acct_u", "pythia_acct_r"] {
            live.try_exec(&format!("drop owned by {role}"));
            live.try_exec(&format!("drop role if exists {role}"));
        }
        live.try_exec("drop table if exists public.pythia_acct_t");
        live.try_exec("drop sequence if exists public.pythia_acct_s");
    };
    cleanup(&mut live);
    live.exec("create table public.pythia_acct_t (id int)");
    live.exec("create sequence public.pythia_acct_s");
    let db = live
        .run(|c| super::texts(c, "select current_database()", &[]))
        .unwrap()
        .remove(0);
    let table = Scope::Object("public".into(), "pythia_acct_t".into());
    let sequence = Scope::Object("public".into(), "pythia_acct_s".into());

    create(&mut live, ObjectType::Role, |r| {
        r.name = "pythia_acct_r".into();
        r.options.insert("CREATEDB".into());
    });

    let mut meant = None;
    let user = create(&mut live, ObjectType::User, |u| {
        u.name = "pythia_acct_u".into();
        u.password = "pw".into();
        u.connection_limit = "3".into();
        u.valid_until = "2031-01-01".into();
        u.options.remove("INHERIT");
        u.roles.insert(
            "pythia_acct_r".into(),
            RoleGrant {
                admin: true,
                default: true,
            },
        );
        u.privileges.insert(
            (Scope::Database(db.clone()), "CONNECT".into()),
            Grant::WithGrantOption,
        );
        u.privileges.insert(
            (Scope::Schema("public".into()), "USAGE".into()),
            Grant::Plain,
        );
        u.privileges
            .insert((table.clone(), "SELECT".into()), Grant::WithGrantOption);
        u.privileges
            .insert((sequence.clone(), "USAGE".into()), Grant::Plain);
        meant = Some(u.clone());
    });
    assert!(user.valid_until.starts_with("2031-01-01"), "{user:#?}");
    let meant = UserDef {
        valid_until: user.valid_until.clone(),
        ..meant.unwrap()
    };
    assert_same(&user, &meant);

    let (loaded, meant) = change(&mut live, "pythia_acct_u", ObjectType::User, |u| {
        u.valid_until.clear();
        u.connection_limit.clear();
        u.options.insert("INHERIT".into());
        u.roles.get_mut("pythia_acct_r").unwrap().admin = false;
        u.privileges.insert(
            (Scope::Database(db.clone()), "TEMPORARY".into()),
            Grant::Plain,
        );
        u.privileges
            .insert((table.clone(), "SELECT".into()), Grant::Plain);
        u.privileges.remove(&(sequence.clone(), "USAGE".into()));
    });
    assert_same(&loaded, &meant);
    assert!(loaded.valid_until.is_empty() && loaded.connection_limit.is_empty());
    cleanup(&mut live);
}

#[test]
fn sqlserver_users_and_roles() {
    let Some(mut live) = Live::connect("PYTHIA_TEST_MSSQL", DbKind::SqlServer) else {
        return;
    };
    let cleanup = |live: &mut Live| {
        live.try_exec("drop user if exists pythia_acct_u");
        live.try_exec("drop role if exists pythia_acct_r");
        live.try_exec("drop table if exists dbo.pythia_acct_t");
    };
    cleanup(&mut live);
    live.exec("create table dbo.pythia_acct_t (id int)");
    let table = Scope::Object("dbo".into(), "pythia_acct_t".into());

    create(&mut live, ObjectType::Role, |r| {
        r.name = "pythia_acct_r".into();
        r.privileges
            .insert((Scope::Schema("dbo".into()), "SELECT".into()), Grant::Plain);
    });

    let mut meant = None;
    let user = create(&mut live, ObjectType::User, |u| {
        u.name = "pythia_acct_u".into();
        u.login = Login::None;
        u.roles.insert("pythia_acct_r".into(), grant());
        u.roles.insert("db_datareader".into(), grant());
        u.privileges
            .insert((Scope::Global, "CREATE TABLE".into()), Grant::Plain);
        u.privileges.insert(
            (Scope::Schema("dbo".into()), "INSERT".into()),
            Grant::WithGrantOption,
        );
        u.privileges
            .insert((table.clone(), "DELETE".into()), Grant::Deny);
        meant = Some(u.clone());
    });
    let mut meant = meant.unwrap();
    // Every user may connect.
    if user
        .privileges
        .contains_key(&(Scope::Global, "CONNECT".into()))
    {
        meant
            .privileges
            .insert((Scope::Global, "CONNECT".into()), Grant::Plain);
    }
    assert_eq!(user.login, Login::None);
    assert_same(&user, &meant);

    let (loaded, meant) = change(&mut live, "pythia_acct_u", ObjectType::User, |u| {
        u.default_schema = "guest".into();
        u.roles.remove("db_datareader");
        u.privileges
            .insert((Scope::Schema("dbo".into()), "INSERT".into()), Grant::Plain);
        u.privileges.remove(&(table.clone(), "DELETE".into()));
        u.privileges
            .insert((table.clone(), "SELECT".into()), Grant::Deny);
    });
    assert_same(&loaded, &meant);
    assert_eq!(loaded.default_schema, "guest");
    cleanup(&mut live);

    // A new login with its user, disabled, then enabled.
    let Some(mut dba) = Live::connect("PYTHIA_TEST_MSSQL_DBA", DbKind::SqlServer) else {
        return;
    };
    let cleanup = |live: &mut Live| {
        live.try_exec("drop user if exists pythia_acct_l");
        live.try_exec("if suser_id('pythia_acct_l') is not null drop login pythia_acct_l");
    };
    cleanup(&mut dba);
    let user = create(&mut dba, ObjectType::User, |u| {
        u.name = "pythia_acct_l".into();
        u.login = Login::New;
        u.password = "Pythia-acct-2026".into();
        u.locked = Some(true);
    });
    assert_eq!(user.login, Login::Mapped);
    assert_eq!(user.login_name, "pythia_acct_l");
    assert_eq!(user.locked, Some(true));
    let (loaded, _) = change(&mut dba, "pythia_acct_l", ObjectType::User, |u| {
        u.locked = Some(false);
        u.password = "Pythia-acct-2027".into();
    });
    assert_eq!(loaded.locked, Some(false));
    cleanup(&mut dba);
}
