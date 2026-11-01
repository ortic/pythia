//! Open connections and their sessions.
//!
//! Every connection has a *main* session used by the object browser and other
//! metadata lookups. Documents (SQL and program windows) get their own session
//! in multi-session mode, so a long-running query never blocks the browser or
//! another window, and every window has its own transaction. Object views and
//! forms load on a *metadata* session of their own, so they don't queue
//! behind the browser, completion and compiles on the main session.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pythia_config::ConnectionProfile;
use pythia_db::ssh::{SshParams, Tunnel};
use pythia_db::{ConnectParams, DbError, DbEvent, DbKind, DbRequest, Dialect, ServerInfo, Session};

use crate::documents::DocId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionKey {
    Main,
    Meta,
    Doc(DocId),
}

pub enum Status {
    Connecting,
    Connected(ServerInfo),
}

pub struct DocSession {
    pub session: Session,
    pub pending_tx: bool,
    pub autocommit: bool,
}

pub struct Connection {
    pub profile: ConnectionProfile,
    params: ConnectParams,
    pub status: Status,
    pub main: Session,
    pub main_pending_tx: bool,
    /// Requests sent to the main session and not answered yet (#215).
    main_pending: HashMap<u64, Answer>,
    /// Answers for requests that a lost main session took with it, for the
    /// next [`Connections::poll`].
    main_failed: Vec<DbEvent>,
    /// The metadata session, opened on first use.
    meta: Option<Session>,
    /// Requests sent to it and not answered yet.
    meta_pending: HashMap<u64, Answer>,
    pub docs: HashMap<DocId, DocSession>,
}

/// The event that answers a request, so the request can be answered when
/// its session is gone.
#[derive(Debug, Clone, Copy)]
enum Answer {
    Meta,
    Task,
    Statement,
}

impl Answer {
    fn of(request: &DbRequest) -> Option<(u64, Answer)> {
        match request {
            DbRequest::Meta { id, .. } => Some((*id, Answer::Meta)),
            DbRequest::Task { id, .. } | DbRequest::Run { id, .. } => Some((*id, Answer::Task)),
            DbRequest::Execute { id, .. } => Some((*id, Answer::Statement)),
            _ => None,
        }
    }

    /// The request whose answer `event` is.
    fn answered(event: &DbEvent) -> Option<u64> {
        match event {
            DbEvent::Meta { id, .. }
            | DbEvent::TaskDone { id, .. }
            | DbEvent::Executed { id, .. }
            | DbEvent::Failed { id, .. }
            | DbEvent::Rows { id, .. } => Some(*id),
            _ => None,
        }
    }

    fn failed(self, id: u64, error: DbError) -> DbEvent {
        match self {
            Answer::Meta => DbEvent::Meta {
                id,
                result: Err(error),
            },
            Answer::Task => DbEvent::TaskDone {
                id,
                result: Err(error),
            },
            Answer::Statement => DbEvent::Failed {
                id,
                error,
                elapsed: Duration::ZERO,
            },
        }
    }
}

impl Connection {
    pub fn name(&self) -> String {
        self.profile.display_name()
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.status, Status::Connected(_))
    }

    pub fn server(&self) -> Option<&ServerInfo> {
        match &self.status {
            Status::Connected(info) => Some(info),
            Status::Connecting => None,
        }
    }

    /// How sessions of this connection log in (for worker sessions).
    pub fn params(&self) -> &pythia_db::ConnectParams {
        &self.params
    }

    /// The connection's database and what it supports.
    pub fn dialect(&self) -> Dialect {
        self.profile.kind.into()
    }

    /// Makes `database` the connection's (MySQL, where it is what
    /// unqualified names refer to): new sessions start in it, and the open
    /// ones switch to it after what they are running.
    pub fn use_database(&mut self, database: &str, notify: &Notify) {
        self.params.init_sql = init_sql(&self.profile, Some(database));
        let statement = format!("use {}", self.dialect().quote_ident(database));
        let use_it = || -> DbRequest {
            let statement = statement.clone();
            DbRequest::Run {
                id: crate::documents::request_id(),
                task: Box::new(move |client| {
                    client.query(&statement, &[])?;
                    Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                }),
            }
        };
        self.send_main(use_it());
        if self.meta.is_some() {
            let request = use_it();
            let DbRequest::Run { id, .. } = &request else {
                unreachable!()
            };
            self.send_meta(*id, request, notify);
        }
        for doc in self.docs.values() {
            doc.session.send(use_it());
        }
        if let Status::Connected(server) = &mut self.status {
            server.current_schema = database.to_owned();
        }
    }

    /// The current database of a MySQL connection, for the toolbar and the
    /// status bar ("No database" without one); none for Oracle, whose
    /// schema is the user's.
    pub fn database_label(&self) -> Option<String> {
        let server = self.server()?;
        (self.dialect() == Dialect::MYSQL).then(|| match server.current_schema.as_str() {
            "" => "No database".to_owned(),
            db => db.to_owned(),
        })
    }

    /// Schema the browser starts in.
    pub fn current_schema(&self) -> String {
        self.server()
            .map(|s| s.current_schema.clone())
            .unwrap_or_else(|| self.dialect().dictionary_name(&self.profile.username))
    }

    /// The document's session, opening it on first use (with `autocommit`).
    pub fn doc_session(
        &mut self,
        doc: DocId,
        autocommit: bool,
        notify: &Notify,
    ) -> &mut DocSession {
        let params = &self.params;
        self.docs.entry(doc).or_insert_with(|| {
            let session = Session::connect(params.clone(), notify.callback());
            if autocommit {
                session.send(DbRequest::SetAutocommit(true));
            }
            DocSession {
                session,
                pending_tx: false,
                autocommit,
            }
        })
    }

    pub fn any_pending_tx(&self) -> bool {
        self.main_pending_tx || self.docs.values().any(|d| d.pending_tx)
    }

    /// Sends a request to the main session, keeping it until it's answered.
    pub fn send_main(&mut self, request: DbRequest) {
        if let Some((id, answer)) = Answer::of(&request) {
            self.main_pending.insert(id, answer);
        }
        self.main.send(request);
    }

    /// Replaces a lost main session with a new one. The requests waiting on
    /// the old one are answered with an error by the next poll: their
    /// answers are gone with it, and the browser waited for its folders
    /// forever (#215). Not a lost connection itself, so it opens no other
    /// session.
    pub fn reconnect_main(&mut self, notify: &Notify) {
        self.main = Session::connect(self.params.clone(), notify.callback());
        self.main_pending_tx = false;
        let error = DbError::new(
            "The connection was lost before this was answered; it is being opened again",
        );
        self.main_failed.extend(
            self.main_pending
                .drain()
                .map(|(id, answer)| answer.failed(id, error.clone())),
        );
        notify.0.request_repaint();
    }

    /// Sends a metadata query or task (with its request id) to the metadata
    /// session, opening it on first use.
    pub fn send_meta(&mut self, id: u64, request: DbRequest, notify: &Notify) {
        let params = &self.params;
        let session = self
            .meta
            .get_or_insert_with(|| Session::connect(params.clone(), notify.callback()));
        if let Some((_, answer)) = Answer::of(&request) {
            self.meta_pending.insert(id, answer);
        }
        session.send(request);
    }

    #[cfg(test)]
    pub fn has_meta_session(&self) -> bool {
        self.meta.is_some()
    }

    /// The metadata session's events. If it is lost or can't connect, the
    /// requests waiting on it fail, and the next one opens a new session.
    fn poll_meta(&mut self) -> Vec<DbEvent> {
        let mut out = Vec::new();
        let Some(session) = &mut self.meta else {
            return out;
        };
        let mut failed = None;
        while let Some(event) = session.try_recv() {
            match event {
                DbEvent::Connected(_) | DbEvent::Notice(_) => {}
                DbEvent::ConnectFailed(e) => failed = Some(e),
                DbEvent::Meta { id, result } => {
                    if let Err(e) = &result
                        && e.is_connection_lost()
                    {
                        failed = Some(e.clone());
                    }
                    self.meta_pending.remove(&id);
                    out.push(DbEvent::Meta { id, result });
                }
                DbEvent::TaskDone { id, result } => {
                    if let Err(e) = &result
                        && e.is_connection_lost()
                    {
                        failed = Some(e.clone());
                    }
                    self.meta_pending.remove(&id);
                    out.push(DbEvent::TaskDone { id, result });
                }
                event => out.push(event),
            }
        }
        if let Some(error) = failed {
            self.meta = None;
            for (id, answer) in self.meta_pending.drain() {
                out.push(answer.failed(id, error.clone()));
            }
        }
        out
    }
}

/// Wakes the UI when a session has news.
#[derive(Clone)]
pub struct Notify(pub egui::Context);

impl Notify {
    pub fn callback(&self) -> impl Fn() + Send + 'static {
        let ctx = self.0.clone();
        move || ctx.request_repaint()
    }
}

/// What a connection logs in with; kept in memory only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Passwords {
    pub database: String,
    /// The SSH password, or the key file's passphrase.
    pub ssh: String,
}

#[derive(Default)]
pub struct Connections {
    /// Keyed by display name, which is also how documents refer to them.
    pub open: BTreeMap<String, Connection>,
}

impl Connections {
    /// Host keys the user trusts for SSH tunnels go to `known_hosts`.
    pub fn connect(
        &mut self,
        profile: ConnectionProfile,
        passwords: Passwords,
        known_hosts: &Path,
        notify: &Notify,
    ) -> String {
        let ssh = &profile.ssh;
        let tunnel = ssh.enabled.then(|| {
            Arc::new(Tunnel::new(SshParams {
                host: ssh.host.clone(),
                username: ssh.username.clone(),
                password: passwords.ssh,
                key_file: key_file(&ssh.key_file),
                known_hosts: known_hosts.to_owned(),
            }))
        });
        let params = ConnectParams {
            kind: profile.kind,
            username: profile.username.clone(),
            password: passwords.database,
            connect_string: profile.connect_string.clone(),
            privilege: profile.privilege,
            init_sql: init_sql(&profile, None),
            ssh: tunnel,
        };
        let name = profile.display_name();
        let main = Session::connect(params.clone(), notify.callback());
        self.open.insert(
            name.clone(),
            Connection {
                profile,
                params,
                status: Status::Connecting,
                main,
                main_pending_tx: false,
                main_pending: HashMap::new(),
                main_failed: Vec::new(),
                meta: None,
                meta_pending: HashMap::new(),
                docs: HashMap::new(),
            },
        );
        name
    }

    pub fn get(&self, name: &str) -> Option<&Connection> {
        self.open.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Connection> {
        self.open.get_mut(name)
    }

    /// Closing drops all sessions, which disconnects (and rolls back) them.
    pub fn close(&mut self, name: &str) -> Option<Connection> {
        self.open.remove(name)
    }

    pub fn close_doc(&mut self, doc: DocId) {
        for conn in self.open.values_mut() {
            if let Some(s) = conn.docs.remove(&doc) {
                s.session.cancel();
            }
        }
    }

    /// Commits or rolls back a window's transaction, then closes its
    /// session (no cancel, which could hit the commit).
    pub fn end_doc(&mut self, doc: DocId, commit: bool) {
        for conn in self.open.values_mut() {
            if let Some(s) = conn.docs.remove(&doc) {
                s.session.send(if commit {
                    DbRequest::Commit
                } else {
                    DbRequest::Rollback
                });
                // Dropping it queues the disconnect after that.
            }
        }
    }

    pub fn any_pending_tx(&self) -> bool {
        self.open.values().any(Connection::any_pending_tx)
    }

    /// Drains all sessions' events.
    pub fn poll(&mut self) -> Vec<(String, SessionKey, DbEvent)> {
        let mut out = Vec::new();
        for (name, conn) in &mut self.open {
            for event in conn.main_failed.drain(..) {
                out.push((name.clone(), SessionKey::Main, event));
            }
            while let Some(event) = conn.main.try_recv() {
                if let DbEvent::Transaction { pending } = event {
                    conn.main_pending_tx = pending;
                }
                if let Some(id) = Answer::answered(&event) {
                    conn.main_pending.remove(&id);
                }
                out.push((name.clone(), SessionKey::Main, event));
            }
            for event in conn.poll_meta() {
                out.push((name.clone(), SessionKey::Meta, event));
            }
            for (doc, s) in &mut conn.docs {
                while let Some(event) = s.session.try_recv() {
                    if let DbEvent::Transaction { pending } = event {
                        s.pending_tx = pending;
                    }
                    out.push((name.clone(), SessionKey::Doc(*doc), event));
                }
            }
        }
        out
    }

    pub fn send_main(&mut self, name: &str, request: DbRequest) -> bool {
        match self.open.get_mut(name) {
            Some(conn) => {
                conn.send_main(request);
                true
            }
            None => false,
        }
    }
}

/// The key file's path, with `~` for the home directory.
fn key_file(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let home = text
        .strip_prefix("~/")
        .or_else(|| text.strip_prefix("~\\"))
        .zip(std::env::home_dir());
    Some(match home {
        Some((rest, home)) => home.join(rest),
        None => PathBuf::from(text),
    })
}

/// Statements every session of `profile` runs after connecting; `database`
/// is the one picked in the object browser (MySQL), over the profile's
/// default schema.
fn init_sql(profile: &ConnectionProfile, database: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let schema = database.unwrap_or(profile.default_schema.trim());
    if !schema.is_empty() {
        let dialect = Dialect::from(profile.kind);
        let schema = dialect.quote_ident(&dialect.dictionary_name(schema));
        match profile.kind {
            DbKind::Oracle => out.push(format!("alter session set current_schema = {schema}")),
            DbKind::MySql => out.push(format!("use {schema}")),
            DbKind::Postgres => out.push(format!("set search_path to {schema}")),
            // The default schema is the database user's; SQLite's is `main`.
            DbKind::SqlServer | DbKind::Sqlite => {}
        }
    }
    let script = &profile.login_script;
    let syntax = Dialect::from(profile.kind).syntax();
    for statement in pythia_plsql::split_statements_in(script, syntax) {
        let text = statement.text(script);
        // SQL*Plus commands mean nothing to a session; on other databases
        // most of them are SQL (MySQL's `SET`).
        let client = statement.kind == pythia_plsql::StatementKind::SqlPlus
            && (profile.kind == DbKind::Oracle || crate::sql_window::client_command(text));
        if !client {
            out.push(text.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Instant;

    use pythia_db::MetaQuery;

    use super::*;

    /// The events of a session by request id (the connect's as 0).
    type Seen = BTreeMap<u64, Vec<DbEvent>>;

    /// Every request sent to a main session gets an answer, also when the
    /// session is lost and replaced while they wait (#215): those queued on
    /// it were dropped, and the browser's folders stayed "loading".
    #[test]
    fn a_replaced_main_session_answers_every_request() {
        let notify = Notify(egui::Context::default());
        let mut conns = Connections::default();
        let profile = ConnectionProfile {
            name: "memory".into(),
            kind: DbKind::Sqlite,
            connect_string: ":memory:".into(),
            ..Default::default()
        };
        let name = conns.connect(
            profile,
            Passwords::default(),
            Path::new("known_hosts"),
            &notify,
        );
        // Events until `done` holds.
        let poll = |conns: &mut Connections, seen: &mut Seen, done: &dyn Fn(&Seen) -> bool| {
            let started = Instant::now();
            while !done(seen) {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "no answer for some of the requests: {seen:?}"
                );
                for (_, key, event) in conns.poll() {
                    assert_eq!(key, SessionKey::Main);
                    let id = match &event {
                        DbEvent::Meta { id, .. } | DbEvent::TaskDone { id, .. } => *id,
                        DbEvent::Connected(_) => 0,
                        _ => continue,
                    };
                    seen.entry(id).or_default().push(event);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let mut seen = BTreeMap::new();
        poll(&mut conns, &mut seen, &|seen| seen.contains_key(&0));

        // The first request fails as when the connection is lost; the
        // others wait behind it, as when several folders are expanded at
        // once.
        let gate = |answer: Result<(), DbError>| {
            let (open, gate) = mpsc::channel::<()>();
            let task: pythia_db::RunFn = Box::new(move |_| {
                let _ = gate.recv();
                answer.map(|()| Box::new(()) as Box<dyn std::any::Any + Send>)
            });
            (open, task)
        };
        let lost = DbError {
            lost: true,
            ..DbError::new("Lost connection")
        };
        let (first, task) = gate(Err(lost));
        conns.send_main(&name, DbRequest::Run { id: 1, task });
        let (second, task) = gate(Ok(()));
        conns.send_main(&name, DbRequest::Run { id: 2, task });
        for id in [3, 4] {
            conns.send_main(
                &name,
                DbRequest::Meta {
                    id,
                    query: MetaQuery::Schemas,
                },
            );
        }
        first.send(()).unwrap();
        poll(&mut conns, &mut seen, &|seen| seen.contains_key(&1));
        // What the app does on a lost error.
        conns.get_mut(&name).unwrap().reconnect_main(&notify);
        let _ = second.send(());
        poll(&mut conns, &mut seen, &|seen| {
            [2, 3, 4].iter().all(|id| seen.contains_key(id))
        });
        for id in [2, 3, 4] {
            let [DbEvent::Meta { result: Err(e), .. } | DbEvent::TaskDone { result: Err(e), .. }] =
                &seen[&id][..]
            else {
                panic!("{id}: {:?}", seen[&id]);
            };
            // Not lost: the app would open yet another session.
            assert!(!e.is_connection_lost(), "{e}");
        }
    }
}
