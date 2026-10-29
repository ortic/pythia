//! Setup shared by the tests against a real database (`PYTHIA_TEST_DB`):
//! the demo schema several tests use, and privileges a DBA grants when
//! `PYTHIA_TEST_DB_DBA` is set, so the tests also pass on a new database.

use std::sync::Mutex;

use pythia_db::oracle::Connection;

/// The objects of [`SCRIPT`]; it runs when one of them is missing.
const OBJECTS: [&str; 4] = [
    "DEMO_DEPARTMENTS",
    "DEMO_EMPLOYEES",
    "DEMO_PKG",
    "DEMO_BROKEN",
];

const SCRIPT: &str = "
begin
  for t in (select table_name from user_tables where table_name in ('DEMO_EMPLOYEES', 'DEMO_DEPARTMENTS')) loop
    execute immediate 'drop table ' || t.table_name || ' cascade constraints purge';
  end loop;
end;
/
create table demo_departments (
  department_id   number(4) primary key,
  department_name varchar2(30 char) not null,
  location        varchar2(40 char)
);
create table demo_employees (
  employee_id   number(6) primary key,
  first_name    varchar2(20 char),
  last_name     varchar2(25 char) not null,
  email         varchar2(40 char) not null unique,
  hire_date     date default sysdate not null,
  salary        number(8,2),
  department_id number(4) references demo_departments
);
comment on column demo_employees.salary is 'Monthly salary in CHF';
insert into demo_departments values (10, 'Administration', 'Zurich');
insert into demo_departments values (20, 'Engineering', 'Basel');
insert into demo_departments values (30, 'Sales', 'Geneva');
insert into demo_employees
  select level, initcap(dbms_random.string('l', 6)), initcap(dbms_random.string('l', 8)),
         'user' || level || '@example.com', date '2015-01-01' + level * 3,
         round(4000 + dbms_random.value * 9000, 2), 10 * (mod(level, 3) + 1)
    from dual connect by level <= 1200;
commit;
create or replace package demo_pkg as
  -- Raises the salary of an employee by a percentage.
  procedure raise_salary(p_employee_id in number, p_percent in number default 5);
  function department_total(p_department_id in number) return number;
end demo_pkg;
/
create or replace package body demo_pkg as
  procedure raise_salary(p_employee_id in number, p_percent in number default 5) is
  begin
    update demo_employees
       set salary = salary * (1 + p_percent / 100)
     where employee_id = p_employee_id;
  end raise_salary;

  function department_total(p_department_id in number) return number is
    l_total number;
  begin
    select sum(salary) into l_total from demo_employees where department_id = p_department_id;
    return l_total;
  end department_total;
end demo_pkg;
/
create or replace procedure demo_broken is
begin
  missing_procedure(42);
end;
/
";

/// A connection to the database of `env` (`user/password@connect
/// string`), if it is set.
pub fn connect(var: &str) -> Option<Connection> {
    let spec = std::env::var(var).ok()?;
    let (credentials, connect_string) = spec.rsplit_once('@')?;
    let (user, password) = credentials.split_once('/')?;
    Some(Connection::connect(user, password, connect_string).expect(var))
}

/// The test user, from `PYTHIA_TEST_DB`.
fn user() -> Option<String> {
    let spec = std::env::var("PYTHIA_TEST_DB").ok()?;
    Some(spec.split_once('/')?.0.to_uppercase())
}

/// Creates the demo schema (tables, `demo_pkg`, the invalid `demo_broken`)
/// in the test user's schema unless it's there, once per test process.
pub fn demo_schema() {
    static DONE: Mutex<bool> = Mutex::new(false);
    let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
    if *done {
        return;
    }
    let Some(conn) = connect("PYTHIA_TEST_DB") else {
        return;
    };
    let present: i64 = conn
        .query_row_as(
            &format!(
                "select count(distinct object_name) from user_objects where object_name in ({})",
                OBJECTS.map(|o| format!("'{o}'")).join(", ")
            ),
            &[],
        )
        .unwrap();
    if present < OBJECTS.len() as i64 {
        for statement in pythia_plsql::split_statements(SCRIPT) {
            let sql = statement.text(SCRIPT);
            // demo_broken compiles with errors on purpose.
            if let Err(e) = conn.execute(sql, &[])
                && !sql.contains("demo_broken")
            {
                panic!("{e}\n{sql}");
            }
        }
    }
    *done = true;
}

/// One test at a time creates and drops users, roles, profiles and
/// directories: in parallel, that DDL deadlocks in the dictionary
/// (ORA-00060).
pub static ACCOUNTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs `statements` as the DBA (`PYTHIA_TEST_DB_DBA`), with `{user}` as
/// the test user; does nothing without a DBA, then a DBA has to have done it.
pub fn as_dba(statements: &[&str]) {
    let (Some(dba), Some(user)) = (connect("PYTHIA_TEST_DB_DBA"), user()) else {
        return;
    };
    for statement in statements {
        let sql = statement.replace("{user}", &user);
        if let Err(e) = dba.execute(&sql, &[]) {
            panic!("{e}\n{sql}");
        }
    }
}

/// Runs `f` with the client of a session of its own to the database of
/// `env` (`user/password@connect string`); none when `env` isn't set.
pub fn run_on<T: Send + 'static>(
    env: &str,
    kind: pythia_config::DbKind,
    f: impl FnOnce(&mut dyn pythia_db::Client) -> T + Send + 'static,
) -> Option<T> {
    use std::sync::mpsc;
    use std::time::Duration;

    use pythia_db::{ConnectParams, DbEvent, DbRequest, Session};

    let spec = std::env::var(env).ok()?;
    let (credentials, connect_string) = spec.rsplit_once('@')?;
    let (username, password) = credentials.split_once('/')?;
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
    let next = || loop {
        if let Some(event) = session.try_recv() {
            return event;
        }
        wake.recv_timeout(Duration::from_secs(60))
            .expect("timed out");
    };
    match next() {
        DbEvent::Connected(_) => {}
        DbEvent::ConnectFailed(e) => panic!("{env}: {e}"),
        _ => panic!("{env}: not connected"),
    }
    session.send(DbRequest::Run {
        id: 1,
        task: Box::new(move |client| Ok(Box::new(f(client)) as Box<dyn std::any::Any + Send>)),
    });
    loop {
        if let DbEvent::TaskDone { result, .. } = next() {
            let any = result.expect("the task ran");
            return Some(*any.downcast::<T>().expect("its result"));
        }
    }
}

/// Runs `statements` on a session of their own to the database of `env`,
/// each on its own outside a transaction (as PostgreSQL's CREATE INDEX
/// CONCURRENTLY wants): their errors, or none when `env` isn't set.
pub fn execute_on(
    env: &str,
    kind: pythia_config::DbKind,
    statements: &[&str],
) -> Option<Vec<Result<(), String>>> {
    let owned: Vec<String> = statements.iter().map(|s| (*s).to_owned()).collect();
    run_on(env, kind, move |client| {
        owned
            .iter()
            .map(|sql| client.execute(sql).map_err(|e| e.to_string()))
            .collect()
    })
}
