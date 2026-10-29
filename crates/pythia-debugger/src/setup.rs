//! What the database needs to connect back to Pythia's debugger, finding
//! out what's missing, and the SQL that allows it.
//!
//! `DBMS_DEBUG_JDWP.CONNECT_TCP(host, port)` makes the database connect to
//! the debugger. That takes `DEBUG CONNECT SESSION`, from 12.1 on a network
//! ACL entry with the JDWP privilege for the user, that exact host string
//! and the port, a host name the database server resolves to this machine,
//! and nothing on the way dropping the connection.
//!
//! - [`Problem`]: which of these a failed `CONNECT_TCP` lacks.
//! - [`facts`]: privileges, ACL entries and addresses, read on the user's
//!   own session.
//! - [`candidates`] and [`probe`]: hosts the database might reach this
//!   machine by, each tried end to end.
//! - [`Grant`] and [`run_as`]: the SQL a DBA runs, and running it.

use std::net::{IpAddr, TcpListener};
use std::ops::RangeInclusive;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use pythia_db::oracle::Connection;
use pythia_db::{ConnectParams, DbError};

use crate::jdwp::Jdwp;

/// Ports the debugger listens on.
pub type Ports = RangeInclusive<u16>;

/// The ports an ACL entry allows when the debugger listens on any free
/// port.
pub const ANY_PORT: Ports = 1024..=65535;

/// The *Debugger ports* setting: empty for any free port, `4000` or
/// `4000-4009`.
pub fn parse_ports(text: &str) -> Result<Option<Ports>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let port = |s: &str| -> Result<u16, String> {
        match s.trim().parse::<u16>() {
            Ok(p) if p >= 1024 => Ok(p),
            Ok(_) => Err("ports below 1024 need administrator rights to listen on".into()),
            Err(_) => Err(format!("{:?} isn't a port (1024 to 65535)", s.trim())),
        }
    };
    let ports = match text.split_once('-') {
        Some((low, high)) => port(low)?..=port(high)?,
        None => port(text)?..=port(text)?,
    };
    if ports.is_empty() {
        return Err("the first port is above the last".into());
    }
    Ok(Some(ports))
}

/// The ports as the setting writes them.
pub fn ports_text(ports: &Ports) -> String {
    if ports.start() == ports.end() {
        ports.start().to_string()
    } else {
        format!("{}-{}", ports.start(), ports.end())
    }
}

/// A listener on all interfaces, on the first free port of `ports` (any
/// free port without).
pub fn listen(ports: Option<&Ports>) -> std::io::Result<TcpListener> {
    let Some(ports) = ports else {
        return TcpListener::bind("0.0.0.0:0");
    };
    let mut last = None;
    for port in ports.clone() {
        match TcpListener::bind(("0.0.0.0", port)) {
            Ok(listener) => return Ok(listener),
            Err(e) => last = Some(e),
        }
    }
    let e = last.expect("a range of at least one port");
    Err(std::io::Error::new(
        e.kind(),
        format!("no free port in {} ({e})", ports_text(ports)),
    ))
}

/// Why `CONNECT_TCP` failed: what the database lacks to reach the
/// debugger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// ORA-01031: the user lacks `DEBUG CONNECT SESSION`.
    NoPrivilege,
    /// ORA-24247: no network ACL entry lets the user connect to this host
    /// and port.
    Acl,
    /// The database server can't resolve the host name (ORA-12545).
    UnknownHost,
    /// Nothing accepted the connection: the name leads to another
    /// machine, or a firewall rejects it (ORA-30698, ORA-12541).
    Refused,
    /// No answer: a firewall or the network drops the connection
    /// (ORA-30699, ORA-12535, ORA-12170, or no connection in time).
    TimedOut,
    /// Another network error (ORA-30683), with Oracle's reason.
    Network(String),
}

impl Problem {
    /// The problem behind an error of `CONNECT_TCP`, if it is one (not,
    /// for example, a lost session).
    pub fn of(error: &DbError) -> Option<Problem> {
        let text = error.text();
        let codes = ora_codes(&text);
        let has = |code: u32| codes.contains(&code);
        Some(if has(24247) {
            Problem::Acl
        } else if has(1031) {
            Problem::NoPrivilege
        } else if has(12545) {
            Problem::UnknownHost
        } else if has(30698) || has(12541) {
            Problem::Refused
        } else if has(30699) || has(12535) || has(12170) {
            Problem::TimedOut
        } else if has(30683) {
            // Oracle's reason is the line after ORA-30683, if any.
            let reason = text
                .lines()
                .skip_while(|l| !l.starts_with("ORA-30683"))
                .nth(1)
                .filter(|l| !l.starts_with("ORA-06512"))
                .unwrap_or("failure establishing connection to debugger");
            Problem::Network(reason.to_owned())
        } else {
            return None;
        })
    }

    /// What it means for the database connecting to `host` on `port`, in
    /// a sentence.
    pub fn explain(&self, host: &str, port: u16) -> String {
        match self {
            Problem::NoPrivilege => {
                "The user lacks DEBUG CONNECT SESSION, which debugging needs.".to_owned()
            }
            Problem::Acl => format!(
                "No network ACL entry lets the database connect to {host} port {port} \
                 for debugging (JDWP)."
            ),
            Problem::UnknownHost => format!("The database server can't resolve {host}."),
            Problem::Refused => format!(
                "{host} refused the connection on port {port}: the name leads to another \
                 machine, or a firewall rejects it."
            ),
            Problem::TimedOut => format!(
                "The connection to {host} port {port} got no answer: a firewall or the \
                 network drops it."
            ),
            Problem::Network(reason) => {
                format!("The database couldn't connect to {host} port {port}: {reason}")
            }
        }
    }

    /// Short, for a table cell.
    pub fn label(&self) -> String {
        match self {
            Problem::NoPrivilege => "no DEBUG CONNECT SESSION".to_owned(),
            Problem::Acl => "no ACL entry allows it".to_owned(),
            Problem::UnknownHost => "the server can't resolve it".to_owned(),
            Problem::Refused => "refused".to_owned(),
            Problem::TimedOut => "no answer (dropped)".to_owned(),
            Problem::Network(reason) => reason.clone(),
        }
    }
}

/// The `ORA-nnnnn` codes in an error's text, in order.
fn ora_codes(text: &str) -> Vec<u32> {
    text.match_indices("ORA-")
        .filter_map(|(at, _)| {
            let digits = text.get(at + 4..at + 9)?;
            digits
                .bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| digits.parse().ok())?
        })
        .collect()
}

/// A JDWP entry of a network ACL that applies to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ace {
    /// The host it names: a name, an address, or a pattern (`*.corp.com`,
    /// `10.1.*`, `*`).
    pub host: String,
    /// The ports it covers (none: all).
    pub ports: Option<Ports>,
    /// GRANT, else DENY.
    pub grant: bool,
}

/// The patterns an ACL entry may name to cover `host`, most specific
/// first, as `DBMS_NETWORK_ACL_UTILITY.DOMAINS` lists them:
/// `dev.corp.com` → `dev.corp.com`, `*.corp.com`, `*.com`, `*`;
/// `10.1.2.3` → `10.1.2.3`, `10.1.2.*`, `10.1.*`, `10.*`, `*`.
pub fn domains(host: &str) -> Vec<String> {
    let host = host.trim().to_lowercase();
    let mut out = vec![host.clone()];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => {
            let parts: Vec<&str> = host.split('.').collect();
            for n in (1..parts.len()).rev() {
                out.push(format!("{}.*", parts[..n].join(".")));
            }
        }
        Ok(IpAddr::V6(_)) => {}
        Err(_) => {
            let mut rest = host.as_str();
            while let Some((_, parent)) = rest.split_once('.') {
                out.push(format!("*.{parent}"));
                rest = parent;
            }
        }
    }
    if host != "*" {
        out.push("*".to_owned());
    }
    out
}

/// Whether the entries let the user connect to `host` on every port of
/// `ports`. The most specific host they name decides, and a DENY there
/// wins (Oracle orders the entries of one host; this is close enough to
/// say whether a fix is needed).
pub fn acl_allows(aces: &[Ace], host: &str, ports: &Ports) -> bool {
    let overlaps = |a: &Ace| {
        a.ports
            .as_ref()
            .is_none_or(|p| p.start() <= ports.end() && ports.start() <= p.end())
    };
    for domain in domains(host) {
        let here: Vec<&Ace> = aces
            .iter()
            .filter(|a| a.host.eq_ignore_ascii_case(&domain) && overlaps(a))
            .collect();
        if here.is_empty() {
            continue;
        }
        if here.iter().any(|a| !a.grant) {
            return false;
        }
        // Granted ranges may cover the ports together.
        let mut ranges: Vec<Ports> = here
            .iter()
            .map(|a| a.ports.clone().unwrap_or(0..=u16::MAX))
            .collect();
        ranges.sort_by_key(|r| *r.start());
        let mut next = u32::from(*ports.start());
        for r in ranges {
            if u32::from(*r.start()) > next {
                break;
            }
            next = next.max(u32::from(*r.end()) + 1);
        }
        return next > u32::from(*ports.end());
    }
    false
}

/// The ports of the entries that allow `host`, if they all allow a range
/// narrower than any port: the debugger has to listen there.
pub fn allowed_ports(aces: &[Ace], host: &str) -> Option<Ports> {
    let level = domains(host).into_iter().find(|d| {
        aces.iter()
            .any(|a| a.grant && a.host.eq_ignore_ascii_case(d))
    })?;
    aces.iter()
        .filter(|a| a.grant && a.host.eq_ignore_ascii_case(&level))
        .map(|a| a.ports.clone())
        .collect::<Option<Vec<Ports>>>()?
        .into_iter()
        .max_by_key(|r| r.end() - r.start())
}

/// What the debugging user has, read on their own session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// The session user, as grants and ACL entries name it.
    pub user: String,
    /// The container (PDB) the session is in (12.1 on).
    pub container: Option<String>,
    /// JDWP connections need a network ACL entry (12.1 on).
    pub acl: bool,
    pub debug_connect: bool,
    pub debug_any: bool,
    /// The client's address as the database server sees it; none for a
    /// local (bequeath) connection.
    pub client_address: Option<String>,
    /// The JDWP entries that apply to the user, if `DBA_HOST_ACES` is
    /// readable (`USER_HOST_ACES` leaves JDWP out).
    pub aces: Option<Vec<Ace>>,
    /// The user may change network ACLs (EXECUTE on
    /// `DBMS_NETWORK_ACL_ADMIN`).
    pub acl_admin: bool,
    /// The user may grant the debug privileges (`GRANT ANY PRIVILEGE`).
    pub grant_any: bool,
}

impl Facts {
    /// Whether the user can run `grant` on their own session.
    pub fn can_run(&self, grant: &Grant) -> bool {
        (!(grant.debug_connect || grant.debug_any) || self.grant_any)
            && (grant.ace.is_none() || self.acl_admin)
    }
}

/// Reads [`Facts`] on the debugging user's session.
pub fn facts(conn: &Connection) -> Result<Facts, DbError> {
    let (user, client_address): (String, Option<String>) = conn.query_row_as(
        "select sys_context('userenv', 'session_user'), sys_context('userenv', 'ip_address') \
         from dual",
        &[],
    )?;
    let acl = pythia_db::compat::at_least(conn, 12, 1)?;
    let container = if acl {
        conn.query_row_as::<Option<String>>(
            "select sys_context('userenv', 'con_name') from dual",
            &[],
        )?
    } else {
        None
    };
    let privs: Vec<String> = conn
        .query_as::<String>(
            "select privilege from session_privs where privilege in \
             ('DEBUG CONNECT SESSION', 'DEBUG ANY PROCEDURE', 'GRANT ANY PRIVILEGE')",
            &[],
        )?
        .collect::<Result<_, _>>()?;
    let has = |p: &str| privs.iter().any(|x| x == p);
    let acl_admin: i64 = conn.query_row_as(
        "select count(*) from all_objects where owner = 'SYS' \
         and object_name = 'DBMS_NETWORK_ACL_ADMIN' and object_type = 'PACKAGE'",
        &[],
    )?;
    Ok(Facts {
        aces: if acl {
            read_aces(conn, &user).ok()
        } else {
            None
        },
        user,
        container,
        acl,
        debug_connect: has("DEBUG CONNECT SESSION"),
        debug_any: has("DEBUG ANY PROCEDURE"),
        client_address: client_address.filter(|a| !a.is_empty()),
        acl_admin: acl_admin > 0,
        grant_any: has("GRANT ANY PRIVILEGE"),
    })
}

/// The JDWP entries for `user`, the roles of the session and PUBLIC.
fn read_aces(conn: &Connection, user: &str) -> Result<Vec<Ace>, DbError> {
    let rows = conn.query_as::<(String, Option<u16>, Option<u16>, String)>(
        "select host, lower_port, upper_port, grant_type from dba_host_aces \
         where privilege = 'JDWP' and (principal = :1 or principal = 'PUBLIC' \
         or principal in (select role from session_roles)) order by host, ace_order",
        &[&user],
    )?;
    let mut aces = Vec::new();
    for row in rows {
        let (host, low, high, grant_type) = row?;
        aces.push(Ace {
            host,
            ports: low.map(|low| low..=high.unwrap_or(low)),
            grant: grant_type == "GRANT",
        });
    }
    Ok(aces)
}

/// A host the database might reach this machine by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub host: String,
    /// Where it comes from.
    pub source: &'static str,
}

/// Hosts to try, best first and each once: the *Debugger host* setting
/// (`setting`, maybe empty), the address the database server sees for this
/// machine, then its full and short host name.
pub fn candidates(setting: &str, client_address: Option<&str>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut add = |host: Option<String>, source| {
        if let Some(host) = host.map(|h| h.trim().to_owned()).filter(|h| !h.is_empty())
            && !out.iter().any(|c| c.host.eq_ignore_ascii_case(&host))
        {
            out.push(Candidate { host, source });
        }
    };
    add(Some(setting.to_owned()), "Debugger host setting");
    add(std::env::var("PYTHIA_DEBUG_HOST").ok(), "PYTHIA_DEBUG_HOST");
    add(
        client_address.map(str::to_owned),
        "the address the database sees for this computer",
    );
    add(full_host_name(), "this computer's full name");
    add(host_name(), "this computer's name");
    out
}

/// This machine's name, without its domain.
pub fn host_name() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| command_output("hostname", &[]))
}

/// This machine's name with its DNS domain, if it has one.
pub fn full_host_name() -> Option<String> {
    let name = if cfg!(windows) {
        let host = std::env::var("COMPUTERNAME").ok()?;
        let domain = std::env::var("USERDNSDOMAIN").ok()?;
        format!("{host}.{}", domain.to_lowercase())
    } else {
        command_output("hostname", &["-f"])?
    };
    name.contains('.').then_some(name)
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// How trying a host went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The database connected, from this address, and the debugger
    /// protocol works (the agent's version).
    Reached {
        port: u16,
        from: IpAddr,
        agent: String,
    },
    /// `CONNECT_TCP` failed, or nothing connected in time.
    Failed {
        port: u16,
        problem: Problem,
        /// Oracle's error, or what happened.
        error: String,
    },
    /// The test couldn't run: no free port, or no session.
    NotRun(String),
}

/// Whether the database connects to the debugger as `host`: listens on
/// `ports`, has a new session of `params` call `CONNECT_TCP(host, port)`
/// and waits up to `timeout` for it (a connection a firewall drops would
/// otherwise take as long as the server's TCP timeout). Blocks.
pub fn probe(
    params: &ConnectParams,
    host: &str,
    ports: Option<&Ports>,
    timeout: Duration,
) -> Probe {
    let listener = match listen(ports).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
        Ok(listener) => listener,
        Err(e) => return Probe::NotRun(format!("Pythia can't listen: {e}")),
    };
    let port = listener.local_addr().map_or(0, |a| a.port());
    let conn = match pythia_db::open_connection(params) {
        Ok(conn) => Arc::new(conn),
        Err(e) => return Probe::NotRun(e.text()),
    };
    let (tx, rx) = mpsc::channel();
    let target = conn.clone();
    let target_host = host.to_owned();
    thread::spawn(move || {
        let result = target
            .execute(
                "begin dbms_debug_jdwp.connect_tcp(:1, :2); end;",
                &[&target_host, &i32::from(port)],
            )
            .map(drop)
            .map_err(DbError::from);
        if result.is_ok() {
            let _ = target.execute("begin dbms_debug_jdwp.disconnect; end;", &[]);
        }
        let _ = tx.send(result);
    });
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, from)) => {
                // The handshake lets CONNECT_TCP return; dispose lets the
                // session go on without a debugger.
                let agent = stream
                    .set_nonblocking(false)
                    .map_err(Into::into)
                    .and_then(|()| Jdwp::handshake(stream))
                    .and_then(|mut jdwp| {
                        let version = jdwp.version();
                        let _ = jdwp.dispose();
                        version
                    });
                let _ = rx.recv_timeout(timeout);
                return match agent {
                    Ok(agent) => Probe::Reached {
                        port,
                        from: from.ip(),
                        agent,
                    },
                    Err(e) => Probe::Failed {
                        port,
                        problem: Problem::Network(format!("debugger protocol: {e}")),
                        error: e.to_string(),
                    },
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Probe::NotRun(format!("Pythia's listener failed: {e}")),
        }
        match rx.try_recv() {
            Ok(Err(e)) => {
                return match Problem::of(&e) {
                    Some(problem) => Probe::Failed {
                        port,
                        problem,
                        error: e.text(),
                    },
                    None => Probe::NotRun(e.text()),
                };
            }
            // Connected, but not here: something else listens on this
            // port where the name leads.
            Ok(Ok(())) => {
                return Probe::Failed {
                    port,
                    problem: Problem::Network("another program accepted the connection".into()),
                    error: String::new(),
                };
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                return Probe::NotRun("the test session ended".into());
            }
        }
        if started.elapsed() >= timeout {
            // The session may only notice once the server gives up.
            let _ = conn.break_execution();
            return Probe::Failed {
                port,
                problem: Problem::TimedOut,
                error: format!("no connection within {} s", timeout.as_secs()),
            };
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// How widely an ACL entry allows hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Exactly the host.
    Host,
    /// Its IPv4 subnet (`10.1.2.*`).
    Subnet,
    /// Its domain (`*.corp.com`).
    Domain,
    /// Any host (`*`).
    Any,
}

impl Scope {
    pub const ALL: [Scope; 4] = [Scope::Host, Scope::Subnet, Scope::Domain, Scope::Any];

    /// The host pattern for `host`, if the scope applies to it (a subnet
    /// to an IPv4 address, a domain to a name with one).
    pub fn pattern(self, host: &str) -> Option<String> {
        let host = host.trim();
        let ipv4 = matches!(host.parse::<IpAddr>(), Ok(IpAddr::V4(_)));
        match self {
            Scope::Host => Some(host.to_owned()),
            Scope::Subnet if ipv4 => host.rsplit_once('.').map(|(net, _)| format!("{net}.*")),
            Scope::Domain if !ipv4 && host.parse::<IpAddr>().is_err() => host
                .split_once('.')
                .map(|(_, domain)| format!("*.{domain}")),
            Scope::Any => Some("*".to_owned()),
            _ => None,
        }
    }
}

/// The SQL that lets `user` debug: the privileges they lack and a network
/// ACL entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub user: String,
    pub debug_connect: bool,
    pub debug_any: bool,
    /// The entry's host pattern and ports; none before 12.1, or when one
    /// allows it already.
    pub ace: Option<(String, Ports)>,
}

impl Grant {
    pub fn is_empty(&self) -> bool {
        !self.debug_connect && !self.debug_any && self.ace.is_none()
    }

    /// The statements, each executable as is.
    pub fn statements(&self) -> Vec<String> {
        let user = user_ident(&self.user);
        let mut out = Vec::new();
        if self.debug_connect {
            out.push(format!("grant debug connect session to {user}"));
        }
        if self.debug_any {
            out.push(format!("grant debug any procedure to {user}"));
        }
        if let Some((host, ports)) = &self.ace {
            out.push(self.ace_call("append_host_ace", host, ports, ""));
        }
        out
    }

    /// The statements that take it back.
    pub fn undo(&self) -> Vec<String> {
        let user = user_ident(&self.user);
        let mut out = Vec::new();
        if let Some((host, ports)) = &self.ace {
            out.push(self.ace_call(
                "remove_host_ace",
                host,
                ports,
                ",\n    remove_empty_acl => true",
            ));
        }
        if self.debug_any {
            out.push(format!("revoke debug any procedure from {user}"));
        }
        if self.debug_connect {
            out.push(format!("revoke debug connect session from {user}"));
        }
        out
    }

    fn ace_call(&self, procedure: &str, host: &str, ports: &Ports, extra: &str) -> String {
        let host = host.replace('\'', "''");
        let principal = self.user.replace('\'', "''");
        format!(
            "begin\n  \
               dbms_network_acl_admin.{procedure}(\n    \
                 host       => '{host}',\n    \
                 lower_port => {},\n    \
                 upper_port => {},\n    \
                 ace        => xs$ace_type(privilege_list => xs$name_list('jdwp'),\n                               \
                                           principal_name => '{principal}',\n                               \
                                           principal_type => xs_acl.ptype_db){extra});\n\
             end;",
            ports.start(),
            ports.end(),
        )
    }
}

/// `user` in a statement: as is if Oracle stores it that way unquoted.
fn user_ident(user: &str) -> String {
    let plain = user.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && user
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || "_$#".contains(c));
    if plain {
        user.to_owned()
    } else {
        format!("\"{}\"", user.replace('"', "\"\""))
    }
}

/// `statements` as a SQL*Plus script.
pub fn script(statements: &[String]) -> String {
    let mut out = String::new();
    for stmt in statements {
        out.push_str(stmt);
        out.push_str(if stmt.starts_with("begin") {
            "\n/\n"
        } else {
            ";\n"
        });
    }
    out
}

/// Runs `statements` on a new session of `params` (a DBA's), in the
/// debugging user's `container` if the session starts in another one (the
/// CDB root, say).
pub fn run_as(
    params: &ConnectParams,
    container: Option<&str>,
    statements: &[String],
) -> Result<(), String> {
    let conn = pythia_db::open_connection(params).map_err(|e| e.text())?;
    if let Some(container) = container {
        let current: Option<String> = conn
            .query_row_as("select sys_context('userenv', 'con_name') from dual", &[])
            .map_err(|e| DbError::from(e).text())?;
        if current.as_deref() != Some(container) {
            let sql = format!(
                "alter session set container = \"{}\"",
                container.replace('"', "\"\"")
            );
            conn.execute(&sql, &[])
                .map_err(|e| format!("Switching to {container}: {}", DbError::from(e).text()))?;
        }
    }
    for stmt in statements {
        if let Err(e) = conn.execute(stmt, &[]) {
            let first = stmt.lines().next().unwrap_or_default();
            return Err(format!("{first}…: {}", DbError::from(e).text()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(text: &str) -> DbError {
        DbError::new(text)
    }

    #[test]
    fn problems() {
        let acl = "ORA-24247: network access denied by access control list (ACL)\n\
                   ORA-06512: at \"SYS.DBMS_DEBUG_JDWP\", line 68\nORA-06512: at line 1";
        assert_eq!(Problem::of(&error(acl)), Some(Problem::Acl));
        let refused = "ORA-30683: failure establishing connection to debugger\n\
                       ORA-30698: network connection failed: connection refused\n\
                       ORA-06512: at \"SYS.DBMS_DEBUG_JDWP\", line 68";
        assert_eq!(Problem::of(&error(refused)), Some(Problem::Refused));
        let unknown = "ORA-30683: failure establishing connection to debugger\n\
                       ORA-12545: Connect failed because target host or object does not exist";
        assert_eq!(Problem::of(&error(unknown)), Some(Problem::UnknownHost));
        let dropped = "ORA-30683: failure establishing connection to debugger\n\
                       ORA-30699: network connection failed: connection timed out";
        assert_eq!(Problem::of(&error(dropped)), Some(Problem::TimedOut));
        // Older releases report TNS errors.
        let old = "ORA-30683: failure establishing connection to debugger\n\
                   ORA-12535: TNS:operation timed out";
        assert_eq!(Problem::of(&error(old)), Some(Problem::TimedOut));
        let other = "ORA-30683: failure establishing connection to debugger\n\
                     ORA-12560: TNS:protocol adapter error\n\
                     ORA-06512: at \"SYS.DBMS_DEBUG_JDWP\", line 68";
        assert_eq!(
            Problem::of(&error(other)),
            Some(Problem::Network(
                "ORA-12560: TNS:protocol adapter error".into()
            ))
        );
        assert_eq!(
            Problem::of(&error("ORA-01031: insufficient privileges")),
            Some(Problem::NoPrivilege)
        );
        assert_eq!(Problem::of(&error("ORA-03113: end-of-file")), None);
        assert_eq!(Problem::of(&error("ORA-1234")), None);
    }

    #[test]
    fn ports() {
        assert_eq!(parse_ports(""), Ok(None));
        assert_eq!(parse_ports(" 4000 "), Ok(Some(4000..=4000)));
        assert_eq!(parse_ports("4000-4009"), Ok(Some(4000..=4009)));
        assert_eq!(parse_ports("4000 - 4009"), Ok(Some(4000..=4009)));
        assert!(parse_ports("80").is_err());
        assert!(parse_ports("4009-4000").is_err());
        assert!(parse_ports("x").is_err());
        assert_eq!(ports_text(&(4000..=4009)), "4000-4009");
        assert_eq!(ports_text(&(4000..=4000)), "4000");
    }

    #[test]
    fn listens_in_range() {
        // Below 32768: no OS hands these out to outgoing connections, which
        // would hold them now and then (#153).
        let first = listen(Some(&(29_100..=29_101))).unwrap();
        let second = listen(Some(&(29_100..=29_101))).unwrap();
        let ports = [
            first.local_addr().unwrap().port(),
            second.local_addr().unwrap().port(),
        ];
        assert!(
            ports.contains(&29_100) && ports.contains(&29_101),
            "{ports:?}"
        );
        let full = listen(Some(&(29_100..=29_101))).unwrap_err();
        assert!(
            full.to_string().contains("no free port in 29100-29101"),
            "{full}"
        );
    }

    #[test]
    fn host_domains() {
        assert_eq!(
            domains("Dev.Corp.Example.com"),
            [
                "dev.corp.example.com",
                "*.corp.example.com",
                "*.example.com",
                "*.com",
                "*"
            ]
        );
        assert_eq!(
            domains("192.168.144.1"),
            ["192.168.144.1", "192.168.144.*", "192.168.*", "192.*", "*"]
        );
        assert_eq!(domains("devbox"), ["devbox", "*"]);
        assert_eq!(domains("::1"), ["::1", "*"]);
    }

    fn ace(host: &str, ports: Option<Ports>, grant: bool) -> Ace {
        Ace {
            host: host.into(),
            ports,
            grant,
        }
    }

    #[test]
    fn acl_matching() {
        let aces = [ace("192.168.144.1", Some(ANY_PORT), true)];
        assert!(acl_allows(&aces, "192.168.144.1", &ANY_PORT));
        assert!(acl_allows(&aces, "192.168.144.1", &(4000..=4009)));
        assert!(!acl_allows(&aces, "devbox", &ANY_PORT));
        // A wildcard covers the subnet; a narrower range only its ports.
        let aces = [ace("10.1.*", Some(4000..=4009), true)];
        assert!(acl_allows(&aces, "10.1.2.3", &(4000..=4005)));
        assert!(!acl_allows(&aces, "10.1.2.3", &ANY_PORT));
        assert_eq!(allowed_ports(&aces, "10.1.2.3"), Some(4000..=4009));
        assert_eq!(allowed_ports(&aces, "10.2.0.1"), None);
        // No ports: all of them. Two ranges can cover it together.
        assert!(acl_allows(&[ace("*", None, true)], "anything", &ANY_PORT));
        assert_eq!(allowed_ports(&[ace("*", None, true)], "anything"), None);
        let split = [
            ace("devbox", Some(1024..=30000), true),
            ace("devbox", Some(30001..=65535), true),
        ];
        assert!(acl_allows(&split, "DEVBOX", &ANY_PORT));
        // The most specific host decides: a DENY there wins over `*`.
        let aces = [ace("*", None, true), ace("devbox", None, false)];
        assert!(!acl_allows(&aces, "devbox", &ANY_PORT));
        assert!(acl_allows(&aces, "other", &ANY_PORT));
    }

    #[test]
    fn candidate_order() {
        let found = candidates(" 192.168.144.1 ", Some("192.168.144.1"));
        assert_eq!(found[0].host, "192.168.144.1");
        assert_eq!(found[0].source, "Debugger host setting");
        // Each host once.
        let hosts: Vec<_> = found.iter().map(|c| c.host.to_lowercase()).collect();
        let mut unique = hosts.clone();
        unique.dedup();
        assert_eq!(hosts.len(), unique.len(), "{hosts:?}");
        let found = candidates("", Some("10.0.0.7"));
        assert!(found.iter().any(|c| c.host == "10.0.0.7"));
        assert!(found.iter().all(|c| !c.host.is_empty()));
    }

    #[test]
    fn scopes() {
        assert_eq!(Scope::Host.pattern("10.1.2.3").as_deref(), Some("10.1.2.3"));
        assert_eq!(
            Scope::Subnet.pattern("10.1.2.3").as_deref(),
            Some("10.1.2.*")
        );
        assert_eq!(Scope::Domain.pattern("10.1.2.3"), None);
        assert_eq!(
            Scope::Domain.pattern("dev.corp.com").as_deref(),
            Some("*.corp.com")
        );
        assert_eq!(Scope::Domain.pattern("devbox"), None);
        assert_eq!(Scope::Subnet.pattern("devbox"), None);
        assert_eq!(Scope::Any.pattern("devbox").as_deref(), Some("*"));
    }

    #[test]
    fn grant_statements() {
        let grant = Grant {
            user: "PYTHIA".into(),
            debug_connect: true,
            debug_any: false,
            ace: Some(("10.1.2.*".into(), 4000..=4009)),
        };
        let sql = grant.statements();
        assert_eq!(sql[0], "grant debug connect session to PYTHIA");
        assert_eq!(sql.len(), 2);
        assert!(sql[1].contains("append_host_ace("), "{}", sql[1]);
        assert!(sql[1].contains("host       => '10.1.2.*'"));
        assert!(sql[1].contains("lower_port => 4000"));
        assert!(sql[1].contains("principal_name => 'PYTHIA'"));
        let undo = grant.undo();
        assert!(undo[0].contains("remove_host_ace("), "{}", undo[0]);
        assert!(undo[0].contains("remove_empty_acl => true"));
        assert_eq!(undo[1], "revoke debug connect session from PYTHIA");
        let script = script(&sql);
        assert!(script.starts_with("grant debug connect session to PYTHIA;\nbegin\n"));
        assert!(script.ends_with("end;\n/\n"));
        // Names Oracle stores in lowercase need quotes in grants.
        let quoted = Grant {
            user: "dev".into(),
            debug_connect: false,
            debug_any: true,
            ace: None,
        };
        assert_eq!(
            quoted.statements(),
            ["grant debug any procedure to \"dev\""]
        );
        assert!(!quoted.is_empty());
    }

    #[test]
    fn what_a_user_can_run() {
        let grant = Grant {
            user: "PYTHIA".into(),
            debug_connect: false,
            debug_any: false,
            ace: Some(("*".into(), ANY_PORT)),
        };
        let mut facts = Facts::default();
        assert!(!facts.can_run(&grant));
        facts.acl_admin = true;
        assert!(facts.can_run(&grant));
        let grants = Grant {
            debug_connect: true,
            ..grant
        };
        assert!(!facts.can_run(&grants));
        facts.grant_any = true;
        assert!(facts.can_run(&grants));
    }
}
