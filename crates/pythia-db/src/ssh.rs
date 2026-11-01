//! SSH tunnels: sessions reach the database through a port forwarded over
//! SSH (like `ssh -L`), the way HeidiSQL connects through SSH.
//!
//! A [`Tunnel`] belongs to a connection, and all its sessions share it. It
//! logs in to the SSH server when the first session connects. Each database
//! address then gets a listener on 127.0.0.1 whose connections are carried
//! over the SSH connection to the server, which connects on to the database.
//! If the SSH connection is lost, the next session that connects logs in
//! again.
//!
//! The server's host key has to be known, from `~/.ssh/known_hosts` or
//! Pythia's own file. An unknown key fails the connect with a [`HostKey`]
//! the user can trust; a changed key just fails.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelOpenFailure, MethodKind, MethodSet};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::types::DbError;

const DEFAULT_PORT: u16 = 22;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// For the whole login: connecting, key exchange and authentication.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How to reach and log in to the SSH server.
#[derive(Clone)]
pub struct SshParams {
    /// `host[:port]` of the SSH server.
    pub host: String,
    pub username: String,
    /// The password, or the private key's passphrase.
    pub password: String,
    /// A private key (OpenSSH, PEM or PuTTY `.ppk`). Without one the
    /// password is used, or the SSH agent when there is no password either.
    pub key_file: Option<PathBuf>,
    /// Host keys trusted in Pythia; `~/.ssh/known_hosts` is read as well.
    pub known_hosts: PathBuf,
}

impl fmt::Debug for SshParams {
    // Never print the password, not even in debug logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshParams")
            .field("host", &self.host)
            .field("username", &self.username)
            .field("key_file", &self.key_file)
            .finish_non_exhaustive()
    }
}

/// A server's host key that isn't known yet. Connecting fails with it (see
/// [`DbError::host_key`]) until the user trusts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    /// The key type and its SHA-256 fingerprint, as `ssh` shows them.
    pub fingerprint: String,
    key: PublicKey,
    known_hosts: PathBuf,
}

impl HostKey {
    fn new(host: &str, port: u16, key: &PublicKey, known_hosts: &Path) -> Self {
        Self {
            host: host.to_owned(),
            port,
            fingerprint: format!(
                "{} {}",
                key.algorithm().as_str(),
                key.fingerprint(HashAlg::Sha256)
            ),
            key: key.clone(),
            known_hosts: known_hosts.to_owned(),
        }
    }

    /// Adds the key to Pythia's known hosts, so it is accepted from now on.
    pub fn trust(&self) -> Result<(), DbError> {
        self.append().map_err(|e| {
            DbError::new(format!(
                "Saving the host key in {}: {e}",
                self.known_hosts.display()
            ))
        })
    }

    fn append(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(dir) = self.known_hosts.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = fs::read_to_string(&self.known_hosts).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let host = if self.port == DEFAULT_PORT {
            self.host.clone()
        } else {
            format!("[{}]:{}", self.host, self.port)
        };
        // Without the key's comment, like `ssh` writes it.
        let key = PublicKey::from(self.key.key_data().clone()).to_openssh()?;
        text.push_str(&format!("{host} {key}\n"));
        fs::write(&self.known_hosts, text)?;
        Ok(())
    }
}

/// The SSH connection of a database connection, shared by its sessions.
pub struct Tunnel {
    params: SshParams,
    state: Mutex<State>,
    /// Why the server didn't open the last forwarded connection.
    refused: Arc<Mutex<Option<String>>>,
}

#[derive(Default)]
struct State {
    /// Started on first use; it runs the SSH connection and the forwards.
    runtime: Option<Runtime>,
    ssh: Option<Arc<Handle<Client>>>,
    /// Local addresses by database address.
    forwards: HashMap<(String, u16), SocketAddr>,
    listeners: Vec<JoinHandle<()>>,
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Tunnel").field(&self.params).finish()
    }
}

impl Tunnel {
    /// Doesn't connect yet: the first [`forward`](Self::forward) does.
    pub fn new(params: SshParams) -> Self {
        Self {
            params,
            state: Mutex::new(State::default()),
            refused: Arc::new(Mutex::new(None)),
        }
    }

    /// A local address that leads to `host:port` as the SSH server sees it,
    /// logging in first if there is no SSH connection (anymore). Blocks
    /// while logging in.
    pub fn forward(&self, host: &str, port: u16) -> Result<SocketAddr, DbError> {
        let mut state = self.state.lock().expect("tunnel mutex");
        let state = &mut *state;
        let runtime = match &mut state.runtime {
            Some(runtime) => runtime,
            None => state.runtime.insert(runtime()?),
        };
        *self.refused.lock().expect("refused mutex") = None;
        let ssh = match &state.ssh {
            Some(ssh) if !ssh.is_closed() => ssh.clone(),
            _ => {
                for listener in state.listeners.drain(..) {
                    listener.abort();
                }
                state.forwards.clear();
                let server = Server::parse(&self.params.host)?;
                info!(params = ?self.params, "logging in over SSH");
                let ssh = runtime
                    .block_on(async {
                        tokio::time::timeout(LOGIN_TIMEOUT, login(&server, &self.params)).await
                    })
                    .map_err(|_| {
                        DbError::new(format!(
                            "SSH: {server} didn't complete the login within {} seconds",
                            LOGIN_TIMEOUT.as_secs()
                        ))
                    })??;
                state.ssh.insert(Arc::new(ssh)).clone()
            }
        };
        let target = (host.to_owned(), port);
        if let Some(local) = state.forwards.get(&target) {
            return Ok(*local);
        }
        let listener = runtime
            .block_on(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
            .and_then(|l| Ok((l.local_addr()?, l)))
            .map_err(|e| DbError::new(format!("SSH: opening a local port: {e}")));
        let (local, listener) = listener?;
        info!(%local, "forwarding {host}:{port} over SSH");
        state.listeners.push(runtime.spawn(accept(
            listener,
            ssh,
            target.clone(),
            self.refused.clone(),
        )));
        state.forwards.insert(target, local);
        Ok(local)
    }

    /// `error` of a connect through the tunnel, or why the SSH server
    /// didn't open the forwarded connection, which says more.
    pub fn explain(&self, error: DbError) -> DbError {
        match self.refused.lock().expect("refused mutex").take() {
            Some(reason) => DbError::new(reason),
            None => error,
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // Ends the forwards and the SSH connection, without waiting.
        if let Ok(state) = self.state.get_mut()
            && let Some(runtime) = state.runtime.take()
        {
            runtime.shutdown_background();
        }
    }
}

fn runtime() -> Result<Runtime, DbError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("ssh-tunnel")
        .enable_all()
        .build()
        .map_err(|e| DbError::new(format!("SSH: starting the tunnel: {e}")))
}

/// Carries each connection to `listener` over a channel of its own.
async fn accept(
    listener: TcpListener,
    ssh: Arc<Handle<Client>>,
    (host, port): (String, u16),
    refused: Arc<Mutex<Option<String>>>,
) {
    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // E.g. out of file descriptors: don't spin.
                warn!("SSH tunnel: accepting a connection: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (ssh, host, refused) = (ssh.clone(), host.clone(), refused.clone());
        tokio::spawn(async move {
            let opened = ssh
                .channel_open_direct_tcpip(
                    host.as_str(),
                    u32::from(port),
                    peer.ip().to_string(),
                    u32::from(peer.port()),
                )
                .await;
            match opened {
                Ok(channel) => {
                    let _ = stream.set_nodelay(true);
                    let mut channel = channel.into_stream();
                    match tokio::io::copy_bidirectional(&mut stream, &mut channel).await {
                        Ok((sent, received)) => {
                            debug!(
                                sent,
                                received, "SSH tunnel: connection to {host}:{port} closed"
                            )
                        }
                        Err(e) => debug!("SSH tunnel: connection to {host}:{port}: {e}"),
                    }
                }
                Err(e) => {
                    let reason = refusal(&host, port, &e);
                    warn!("{reason}");
                    // Before `stream` closes, so the driver's error finds it.
                    *refused.lock().expect("refused mutex") = Some(reason);
                }
            }
        });
    }
}

fn refusal(host: &str, port: u16, error: &russh::Error) -> String {
    match error {
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited) => {
            format!(
                "SSH: the server doesn't allow forwarding to {host}:{port} (AllowTcpForwarding, \
                 PermitOpen in its sshd_config)"
            )
        }
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => format!(
            "SSH: the server couldn't connect to {host}:{port}. The database address is \
             resolved on the SSH server: localhost is the SSH server itself."
        ),
        e => format!("SSH: forwarding to {host}:{port} failed: {e}"),
    }
}

/// The SSH server's address.
struct Server {
    host: String,
    port: u16,
}

impl Server {
    /// `host`, `host:port` or `[ipv6]:port`.
    fn parse(text: &str) -> Result<Self, DbError> {
        let text = text.trim();
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (host, rest) = rest
                .split_once(']')
                .ok_or_else(|| DbError::new(format!("SSH: missing ']' in {text}")))?;
            (host, rest.strip_prefix(':'))
        } else {
            match text.rsplit_once(':') {
                Some((host, port)) if !host.contains(':') => (host, Some(port)),
                // A bare IPv6 address.
                _ => (text, None),
            }
        };
        if host.is_empty() {
            return Err(DbError::new("SSH: no server given"));
        }
        let port = match port {
            Some(port) => port
                .parse()
                .map_err(|_| DbError::new(format!("SSH: not a port number: {port}")))?,
            None => DEFAULT_PORT,
        };
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }
}

impl fmt::Display for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.port == DEFAULT_PORT {
            f.write_str(&self.host)
        } else if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

async fn login(server: &Server, params: &SshParams) -> Result<Handle<Client>, DbError> {
    let socket = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((server.host.as_str(), server.port)),
    )
    .await
    .map_err(|_| {
        DbError::new(format!(
            "SSH: no answer from {server} within {} seconds",
            CONNECT_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| DbError::new(format!("SSH: can't connect to {server}: {e}")))?;
    let _ = socket.set_nodelay(true);
    let config = Arc::new(client::Config {
        // Keeps NAT and firewalls from dropping an idle tunnel, and notices
        // a dead one.
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    let rejected = Arc::new(Mutex::new(None));
    let client = Client {
        host: server.host.clone(),
        port: server.port,
        known_hosts: params.known_hosts.clone(),
        rejected: rejected.clone(),
    };
    let mut ssh = client::connect_stream(config, socket, client)
        .await
        .map_err(|e| match rejected.lock().expect("rejected mutex").take() {
            Some(error) => error,
            None => DbError::new(format!("SSH: {server}: {e}")),
        })?;
    authenticate(&mut ssh, server, params).await?;
    info!("logged in to {server} over SSH");
    Ok(ssh)
}

async fn authenticate(
    ssh: &mut Handle<Client>,
    server: &Server,
    params: &SshParams,
) -> Result<(), DbError> {
    let user = params.username.trim();
    if user.is_empty() {
        return Err(DbError::new(
            "SSH: enter the user to log in to the SSH server as",
        ));
    }
    let failed = |e: russh::Error| DbError::new(format!("SSH: logging in to {server}: {e}"));
    let (tried, result) = if let Some(path) = &params.key_file {
        let key = load_key(path, &params.password)?;
        let hash = ssh
            .best_supported_rsa_hash()
            .await
            .map_err(failed)?
            .flatten();
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        let result = ssh
            .authenticate_publickey(user, key)
            .await
            .map_err(failed)?;
        ("the key file", result)
    } else if !params.password.is_empty() {
        let result = password(ssh, user, &params.password)
            .await
            .map_err(failed)?;
        ("the password", result)
    } else {
        ("the SSH agent's keys", agent(ssh, user).await?)
    };
    match result {
        AuthResult::Success => Ok(()),
        AuthResult::Failure {
            remaining_methods, ..
        } => Err(DbError::new(format!(
            "SSH: {server} didn't accept {tried} for {user}{}",
            accepted(&remaining_methods)
        ))),
    }
}

/// What the server would accept instead, if it said.
fn accepted(methods: &MethodSet) -> String {
    let names: Vec<&str> = methods
        .iter()
        .filter(|m| **m != MethodKind::None)
        .map(<&str>::from)
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!(" (it accepts: {})", names.join(", "))
    }
}

fn load_key(path: &Path, passphrase: &str) -> Result<russh::keys::PrivateKey, DbError> {
    let passphrase = Some(passphrase).filter(|p| !p.is_empty());
    russh::keys::load_secret_key(path, passphrase).map_err(|e| match e {
        russh::keys::Error::KeyIsEncrypted => DbError::new(format!(
            "SSH: the key file {} is protected by a passphrase: enter it as the SSH password",
            path.display()
        )),
        // A wrong passphrase fails in the cipher, as "cryptographic error"
        // or "Padding error" (#194): the key is encrypted, and it doesn't
        // open it.
        _ if passphrase.is_some()
            && matches!(
                russh::keys::load_secret_key(path, None),
                Err(russh::keys::Error::KeyIsEncrypted)
            ) =>
        {
            DbError::new(format!(
                "SSH: the passphrase doesn't open the key file {}: enter the key's passphrase as \
                 the SSH password",
                path.display()
            ))
        }
        e => DbError::new(format!("SSH: reading the key file {}: {e}", path.display())),
    })
}

/// Password authentication, and keyboard-interactive with the password for
/// servers that only offer that.
async fn password(
    ssh: &mut Handle<Client>,
    user: &str,
    password: &str,
) -> Result<AuthResult, russh::Error> {
    let result = ssh.authenticate_password(user, password).await?;
    let AuthResult::Failure {
        remaining_methods, ..
    } = &result
    else {
        return Ok(result);
    };
    if !remaining_methods.contains(&MethodKind::KeyboardInteractive) {
        return Ok(result);
    }
    let mut reply = ssh
        .authenticate_keyboard_interactive_start(user, None)
        .await?;
    // A server could keep asking; a password login takes a round or two.
    for _ in 0..4 {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(AuthResult::Success),
            KeyboardInteractiveAuthResponse::Failure {
                remaining_methods,
                partial_success,
            } => {
                return Ok(AuthResult::Failure {
                    remaining_methods,
                    partial_success,
                });
            }
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                // Hidden prompts ask for the password; there is nothing to
                // answer others (e.g. a one-time code) with.
                let answers = prompts
                    .iter()
                    .map(|p| {
                        if p.echo {
                            String::new()
                        } else {
                            password.to_owned()
                        }
                    })
                    .collect();
                reply = ssh
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
    Ok(result)
}

/// Logs in with the keys of the SSH agent (Pageant or OpenSSH's on Windows).
async fn agent(ssh: &mut Handle<Client>, user: &str) -> Result<AuthResult, DbError> {
    let (mut agent, identities) = agent_keys().await.map_err(|e| {
        DbError::new(format!(
            "SSH: enter a password or a key file. Without them the SSH agent's keys are used, \
             but there is no agent: {e}"
        ))
    })?;
    let hash = ssh
        .best_supported_rsa_hash()
        .await
        .map_err(|e| DbError::new(format!("SSH: {e}")))?
        .flatten();
    let mut result = AuthResult::Failure {
        remaining_methods: MethodSet::empty(),
        partial_success: false,
    };
    for identity in identities {
        let AgentIdentity::PublicKey { key, comment } = identity else {
            continue;
        };
        match ssh
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            Ok(AuthResult::Success) => return Ok(AuthResult::Success),
            Ok(failure) => result = failure,
            Err(e) => warn!("SSH agent couldn't sign with {comment}: {e}"),
        }
    }
    Ok(result)
}

type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

/// The SSH agent and its keys.
async fn agent_keys() -> Result<(Agent, Vec<AgentIdentity>), russh::keys::Error> {
    #[cfg(windows)]
    {
        // Pageant (PuTTY) first, then the agent of Windows' OpenSSH.
        if let Ok(pageant) = AgentClient::connect_pageant().await {
            let mut pageant = pageant.dynamic();
            if let Ok(keys) = pageant.request_identities().await
                && !keys.is_empty()
            {
                return Ok((pageant, keys));
            }
        }
        let mut agent = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
            .await?
            .dynamic();
        let keys = agent.request_identities().await?;
        Ok((agent, keys))
    }
    #[cfg(not(windows))]
    {
        let mut agent = AgentClient::connect_env().await?.dynamic();
        let keys = agent.request_identities().await?;
        Ok((agent, keys))
    }
}

/// Checks the server's host key.
struct Client {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    /// Why the key was rejected, for the connect error.
    rejected: Arc<Mutex<Option<DbError>>>,
}

impl Client {
    fn check(&self, key: &PublicKey) -> Result<(), DbError> {
        let server = Server {
            host: self.host.clone(),
            port: self.port,
        };
        let user_file = std::env::home_dir().map(|home| home.join(".ssh").join("known_hosts"));
        for file in user_file.iter().chain([&self.known_hosts]) {
            match russh::keys::known_hosts::check_known_hosts_path(&self.host, self.port, key, file)
            {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(russh::keys::Error::KeyChanged { line }) => {
                    return Err(DbError::new(format!(
                        "SSH: the host key of {server} isn't the one in {}, line {}. \
                         Someone could be intercepting the connection. If the server got a new \
                         key, remove that line.",
                        file.display(),
                        file_line(file, line)
                    )));
                }
                Err(e) => warn!("reading {}: {e}", file.display()),
            }
        }
        Err(DbError::unknown_host_key(
            server,
            HostKey::new(&self.host, self.port, key, &self.known_hosts),
        ))
    }
}

/// The line number in `file` of russh's `entry`, which doesn't count
/// comment lines.
fn file_line(file: &Path, entry: usize) -> usize {
    let text = fs::read_to_string(file).unwrap_or_default();
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.starts_with('#'))
        .nth(entry.saturating_sub(1))
        .map_or(entry, |(index, _)| index + 1)
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // A host certificate is checked like its key: the key is what the
        // user trusts.
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        match self.check(&key) {
            Ok(()) => Ok(true),
            Err(error) => {
                *self.rejected.lock().expect("rejected mutex") = Some(error);
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers() {
        let s = Server::parse("bastion").unwrap();
        assert_eq!(
            (s.host.as_str(), s.port, s.to_string()),
            ("bastion", 22, "bastion".into())
        );
        let s = Server::parse(" bastion.example.com:2222 ").unwrap();
        assert_eq!((s.host.as_str(), s.port), ("bastion.example.com", 2222));
        assert_eq!(s.to_string(), "bastion.example.com:2222");
        let s = Server::parse("[::1]:2222").unwrap();
        assert_eq!(
            (s.host.as_str(), s.port, s.to_string()),
            ("::1", 2222, "[::1]:2222".into())
        );
        let s = Server::parse("fe80::1").unwrap();
        assert_eq!((s.host.as_str(), s.port), ("fe80::1", 22));
        assert!(Server::parse("").is_err());
        assert!(Server::parse("host:ssh").is_err());
    }
}
