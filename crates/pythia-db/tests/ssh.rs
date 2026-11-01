//! SSH tunnels against an SSH server in the test process (russh's): host
//! keys, the ways to log in, forwarding to a local echo server, refused
//! forwards, and logging in again after the SSH connection was lost.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use pythia_db::DbError;
use pythia_db::ssh::{SshParams, Tunnel};
use russh::keys::PrivateKey;
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Response, Session, VerifiedPublicKey};
use russh::{Channel, ChannelOpenFailure, MethodKind, MethodSet};

// Keys made for these tests.
const HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACClEhoSIagwuB36qjpnnthtjKcuq3+bXd9hkstg8x23vQAAAIi5usgHubrI
BwAAAAtzc2gtZWQyNTUxOQAAACClEhoSIagwuB36qjpnnthtjKcuq3+bXd9hkstg8x23vQ
AAAEBNRdzTUJqyUfuM1ubN9qtwRxGXCiHQmKchWlpyrvnjLKUSGhIhqDC4HfqqOmee2G2M
py6rf5td32GSy2DzHbe9AAAABWhvc3Qx
-----END OPENSSH PRIVATE KEY-----
";
/// Another server's key, for a known host whose key changed.
const OTHER_HOST_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILL5zYhEdr+KkeU9t+4JRP5rectIJbhpMfrr1n078P/z";
const CLIENT_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACAbY22pGQAIYMGsyO7jJTOzzC6k4wk09tMdEXyMC0/v8AAAAJDfyNTG38jU
xgAAAAtzc2gtZWQyNTUxOQAAACAbY22pGQAIYMGsyO7jJTOzzC6k4wk09tMdEXyMC0/v8A
AAAEBVQLxXdjxITDjOZBjt5FroC7D3RmIMHNpmHM2SpWMnuRtjbakZAAhgwazI7uMlM7PM
LqTjCTT20x0RfIwLT+/wAAAABmNsaWVudAECAwQFBgc=
-----END OPENSSH PRIVATE KEY-----
";
/// An RSA key in PEM (`BEGIN RSA PRIVATE KEY`) with the passphrase
/// `secret-phrase`; a wrong one fails in the padding.
const RSA_PEM_ENCRYPTED: &str = "-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-128-CBC,1936B01A6061FEA2F3D098D20A73FD3E

vs6iZ9PFPDoYeNLAbDsPAEvTg1ED1IUlBBvZtX+ih+K73VuYFlF/qp4+pm9BWRCe
wvhd0EFHgykZyTVlLte3Zdhx4Lw8Hc4OhH8W+j2PiPf5q1d8okyKNqhWUBTLreAU
WuiiJSoMPry9AYkxGZC8vsEFqO4F2TG7GzMb14d7wNv03j9rDFx+Bqkqp8IcL47t
6UFQaQAq62Drg6u1d7VJaTfwMisiCiRXuAiXAyk4mHwiDD+/eSG6qybdFvf8tuHb
5VSGvzxjMOKRhf/2DuMK3JLg+5RVlrPSDHYrwSkgUgQ4Kooeib6W0kLjVTs0Yjr2
i39rDmlLHbcJZAHtJcqtYXb2WxpnRk6GhiBi6hdY3XE9dtI0xhzL7HofnUrFlh8i
M2TYQTpXMqMGH0JLLBEFZ9Fw98MYZ+8K8SZZtkiwUXCg3Xboxx2v/D9Aw/Oywyf6
bFzwBpuEub8bIiKbKL+/b51rNRFEFn/1sO9xPtRiN6X1Rdu64YurFwO4XOww41n0
Xy+ah/+3AXyiVb2kqOcvysRRY3KZHdtw1mRoI8TlZCeTbSI6FcF1qanVxFGPdSyE
96SbPTJ5d/bIX8PIFoKBQdOycBBZVUB9muh0aJ73OeUslqywGpOy8MXH5fyApSlu
2Rakkxk1TW5NfcnYbAdZjOdTKPh84/bsqt0YyYBkMt9LWRywmPwsivaSrqGleLOD
IIevSXsiQQeAirPb2ZmQjAjxAEK+IzjJo/xqaiz2hbVaFZU+YDBJO0Ku4ApmTkl2
fmwHd6HRP0WXkJ8LXGI5hH9RW+p0WwU4YMu+TWdCaJG+F8PwZHTBYWEPXWsUTo4G
-----END RSA PRIVATE KEY-----
";
/// `CLIENT_KEY` with the passphrase `secret-phrase`.
const CLIENT_KEY_ENCRYPTED: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABDAcFWMTM
aJhF891+0ZxtSoAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIBtjbakZAAhgwazI
7uMlM7PMLqTjCTT20x0RfIwLT+/wAAAAkGGmKVh4DFJMsy6aGhYj1pRZ6c7wync+82mD3Q
kM1tsLJzBJX0g4tGGMDn19RnTomPFLqetCmTMv/ixRJtmWCJTgxT6BzaEZATi31pLtruXW
hNI1OM0fROmteis9qmPB1wkxFVIhNWowjMUsuFggAKDTPSLvnDSkeDSZflzOCepzx0fzRM
tluh+8L79/JRzb/g==
-----END OPENSSH PRIVATE KEY-----
";

/// A port that sends back what it receives.
fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut reader = stream.try_clone().unwrap();
                let mut writer = stream;
                let _ = std::io::copy(&mut reader, &mut writer);
            });
        }
    });
    addr
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Sends a line through `addr` and reads it back.
fn echo(addr: SocketAddr) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(b"hello\n")?;
    let mut line = [0; 6];
    stream.read_exact(&mut line)?;
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// An SSH server for the user `pythia`, with the password `secret` or
/// `CLIENT_KEY`. Port 1 stands for a database it may not forward to.
struct SshServer {
    addr: SocketAddr,
    sessions: Arc<Mutex<Vec<server::Handle>>>,
    /// Successful logins.
    logins: Arc<AtomicUsize>,
    runtime: tokio::runtime::Runtime,
}

impl SshServer {
    fn start(methods: &[MethodKind]) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let config = Arc::new(server::Config {
            keys: vec![PrivateKey::from_openssh(HOST_KEY).unwrap()],
            methods: MethodSet::from(methods),
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let logins = Arc::new(AtomicUsize::new(0));
        let (all, count) = (sessions.clone(), logins.clone());
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (config, all) = (config.clone(), all.clone());
                let handler = Handler {
                    logins: count.clone(),
                };
                tokio::spawn(async move {
                    if let Ok(running) = server::run_stream(config, stream, handler).await {
                        all.lock().unwrap().push(running.handle());
                        let _ = running.await;
                    }
                });
            }
        });
        Self {
            addr,
            sessions,
            logins,
            runtime,
        }
    }

    /// Drops the SSH connections, as when the network goes away.
    fn kick(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock().unwrap());
        self.runtime.block_on(async {
            for session in sessions {
                let _ = session
                    .disconnect(
                        russh::Disconnect::ByApplication,
                        "kicked".into(),
                        String::new(),
                    )
                    .await;
            }
        });
    }

    fn params(&self, dir: &Path) -> SshParams {
        SshParams {
            host: self.addr.to_string(),
            username: "pythia".into(),
            password: "secret".into(),
            key_file: None,
            known_hosts: dir.join("known_hosts"),
        }
    }
}

struct Handler {
    logins: Arc<AtomicUsize>,
}

impl Handler {
    fn verdict(&self, accept: bool) -> Auth {
        if accept {
            self.logins.fetch_add(1, Ordering::SeqCst);
            Auth::Accept
        } else {
            Auth::reject()
        }
    }
}

impl server::Handler for Handler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(self.verdict(user == "pythia" && password == "secret"))
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let client = PrivateKey::from_openssh(CLIENT_KEY).unwrap();
        Ok(self.verdict(
            user == "pythia" && key.public_key.key_data() == client.public_key().key_data(),
        ))
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        Ok(match response {
            None => Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Password: ".into(), false)].into(),
            },
            Some(mut answers) => {
                let password = answers.next();
                self.verdict(user == "pythia" && password.as_deref() == Some(&b"secret"[..]))
            }
        })
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if port == 1 {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        match tokio::net::TcpStream::connect((host, port as u16)).await {
            Ok(mut target) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut channel = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut channel, &mut target).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pythia-ssh-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Trusts the server's host key, as the user does on the first connect.
fn trust(server: &SshServer, dir: &Path) {
    let error = Tunnel::new(server.params(dir))
        .forward("127.0.0.1", 9)
        .unwrap_err();
    error
        .host_key
        .expect("an unknown host key")
        .trust()
        .unwrap();
}

fn forward_error(params: SshParams) -> DbError {
    Tunnel::new(params)
        .forward("127.0.0.1", 9)
        .expect_err("the login should fail")
}

#[test]
fn trust_the_host_key_then_forward() {
    let server = SshServer::start(&[MethodKind::Password, MethodKind::PublicKey]);
    let target = echo_server();
    let dir = temp_dir("trust");
    let tunnel = Tunnel::new(server.params(&dir));

    let error = tunnel.forward("127.0.0.1", target.port()).unwrap_err();
    let key = error.host_key.clone().expect("an unknown host key");
    assert!(
        key.fingerprint.starts_with("ssh-ed25519 SHA256:"),
        "{}",
        key.fingerprint
    );
    assert!(error.message.contains(&key.fingerprint), "{error}");
    assert_eq!(server.logins.load(Ordering::SeqCst), 0, "no login before");
    key.trust().unwrap();
    let saved = fs::read_to_string(dir.join("known_hosts")).unwrap();
    assert_eq!(
        saved,
        format!(
            "[127.0.0.1]:{} ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKUSGhIhqDC4HfqqOmee2G2Mpy6rf5td32GSy2DzHbe9\n",
            server.addr.port()
        )
    );

    let local = tunnel.forward("127.0.0.1", target.port()).unwrap();
    assert!(local.ip().is_loopback());
    assert_eq!(echo(local).unwrap(), "hello\n");
    // Connections at the same time share the SSH connection.
    let both = thread::scope(|s| {
        let a = s.spawn(|| echo(local));
        let b = s.spawn(|| echo(local));
        (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
    });
    assert_eq!(both, ("hello\n".into(), "hello\n".into()));
    assert_eq!(tunnel.forward("127.0.0.1", target.port()).unwrap(), local);
    assert_eq!(server.logins.load(Ordering::SeqCst), 1, "one SSH login");

    // Dropping the tunnel closes its ports.
    drop(tunnel);
    let started = Instant::now();
    while echo(local).is_ok() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "port still open"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_changed_host_key_fails() {
    let server = SshServer::start(&[MethodKind::Password]);
    let dir = temp_dir("changed");
    fs::write(
        dir.join("known_hosts"),
        format!(
            "# earlier\n[127.0.0.1]:{} {OTHER_HOST_PUBLIC_KEY}\n",
            server.addr.port()
        ),
    )
    .unwrap();
    let error = forward_error(server.params(&dir));
    assert!(error.host_key.is_none(), "can't be trusted from the dialog");
    assert!(error.message.contains("isn't the one in"), "{error}");
    assert!(error.message.contains("line 2"), "{error}");
}

#[test]
fn logins() {
    let server = SshServer::start(&[MethodKind::Password, MethodKind::PublicKey]);
    let dir = temp_dir("logins");
    trust(&server, &dir);
    let target = echo_server();

    let error = forward_error(SshParams {
        password: "wrong".into(),
        ..server.params(&dir)
    });
    let refused = format!("SSH: {} didn't accept the password for pythia", server.addr);
    assert!(error.message.starts_with(&refused), "{error}");
    assert!(error.message.contains("(it accepts: publickey"), "{error}");
    let error = forward_error(SshParams {
        username: " ".into(),
        ..server.params(&dir)
    });
    assert!(error.message.contains("enter the user"), "{error}");

    let key_file = dir.join("id_ed25519");
    fs::write(&key_file, CLIENT_KEY).unwrap();
    let tunnel = Tunnel::new(SshParams {
        password: String::new(),
        key_file: Some(key_file),
        ..server.params(&dir)
    });
    let local = tunnel.forward("127.0.0.1", target.port()).unwrap();
    assert_eq!(echo(local).unwrap(), "hello\n");

    let encrypted = dir.join("id_encrypted");
    fs::write(&encrypted, CLIENT_KEY_ENCRYPTED).unwrap();
    let error = forward_error(SshParams {
        password: String::new(),
        key_file: Some(encrypted.clone()),
        ..server.params(&dir)
    });
    assert!(
        error.message.contains("protected by a passphrase"),
        "{error}"
    );
    let tunnel = Tunnel::new(SshParams {
        password: "secret-phrase".into(),
        key_file: Some(encrypted.clone()),
        ..server.params(&dir)
    });
    let local = tunnel.forward("127.0.0.1", target.port()).unwrap();
    assert_eq!(echo(local).unwrap(), "hello\n");
    // A wrong passphrase says so (#194), not "cryptographic error" or
    // "Padding error".
    let rsa = dir.join("id_rsa");
    fs::write(&rsa, RSA_PEM_ENCRYPTED).unwrap();
    for key in [encrypted, rsa] {
        let error = forward_error(SshParams {
            password: "wrong".into(),
            key_file: Some(key),
            ..server.params(&dir)
        });
        assert!(
            error
                .message
                .contains("the passphrase doesn't open the key file"),
            "{error}"
        );
    }

    let error = forward_error(SshParams {
        key_file: Some(dir.join("missing")),
        ..server.params(&dir)
    });
    assert!(error.message.contains("reading the key file"), "{error}");

    let error = forward_error(SshParams {
        host: format!("127.0.0.1:{}", closed_port()),
        ..server.params(&dir)
    });
    assert!(
        error
            .message
            .starts_with("SSH: can't connect to 127.0.0.1:"),
        "{error}"
    );
}

#[test]
fn keyboard_interactive_gets_the_password() {
    let server = SshServer::start(&[MethodKind::KeyboardInteractive]);
    let dir = temp_dir("kbdint");
    trust(&server, &dir);
    let target = echo_server();
    let tunnel = Tunnel::new(server.params(&dir));
    let local = tunnel.forward("127.0.0.1", target.port()).unwrap();
    assert_eq!(echo(local).unwrap(), "hello\n");

    let error = forward_error(SshParams {
        password: "wrong".into(),
        ..server.params(&dir)
    });
    assert!(
        error.message.contains("didn't accept the password"),
        "{error}"
    );
}

#[test]
fn refused_forwards_are_explained() {
    let server = SshServer::start(&[MethodKind::Password]);
    let dir = temp_dir("refused");
    trust(&server, &dir);
    let tunnel = Tunnel::new(server.params(&dir));

    for (port, reason) in [
        (1, "doesn't allow forwarding to 127.0.0.1:1"),
        (closed_port(), "couldn't connect to 127.0.0.1:"),
    ] {
        let local = tunnel.forward("127.0.0.1", port).unwrap();
        // The driver sees its connection closed …
        assert!(echo(local).is_err());
        // … and the tunnel says why.
        let error = tunnel.explain(DbError::new("Lost connection to the server"));
        assert!(error.message.contains(reason), "{error}");
        let error = tunnel.explain(DbError::new("Access denied"));
        assert_eq!(error.message, "Access denied", "explained once");
    }
}

#[test]
fn logs_in_again_after_losing_the_connection() {
    let server = SshServer::start(&[MethodKind::Password]);
    let dir = temp_dir("again");
    trust(&server, &dir);
    let target = echo_server();
    let tunnel = Tunnel::new(server.params(&dir));
    let local = tunnel.forward("127.0.0.1", target.port()).unwrap();
    assert_eq!(echo(local).unwrap(), "hello\n");

    server.kick();
    // The next connect (a reconnecting session) logs in again.
    let started = Instant::now();
    loop {
        let local = tunnel.forward("127.0.0.1", target.port()).unwrap();
        if echo(local).is_ok() && server.logins.load(Ordering::SeqCst) == 2 {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "no new login");
        thread::sleep(Duration::from_millis(50));
    }
}
