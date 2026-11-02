//! The `oracle` crate (ODPI-C, needs the Oracle Instant Client) as
//! [`crate::oracle`], with the `thick` feature: its connects wait for other
//! test runs first, as the pure-Rust driver's do (#100). The tests that
//! connect to Oracle without a [`crate::Session`] didn't in thick builds.

pub use ::oracle::*;

/// `oracle::Connection`, with its methods through `Deref`.
#[derive(Debug)]
pub struct Connection(::oracle::Connection);

impl Connection {
    pub fn connect<U, P, C>(username: U, password: P, connect_string: C) -> Result<Connection>
    where
        U: AsRef<str>,
        P: AsRef<str>,
        C: AsRef<str>,
    {
        crate::wait_for_other_test_runs();
        ::oracle::Connection::connect(username, password, connect_string).map(Connection)
    }
}

impl std::ops::Deref for Connection {
    type Target = ::oracle::Connection;

    fn deref(&self) -> &::oracle::Connection {
        &self.0
    }
}

/// `oracle::Connector`, for a privilege or external authentication.
#[derive(Debug, Clone)]
pub struct Connector(::oracle::Connector);

impl Connector {
    pub fn new<U, P, C>(username: U, password: P, connect_string: C) -> Connector
    where
        U: Into<String>,
        P: Into<String>,
        C: Into<String>,
    {
        Connector(::oracle::Connector::new(username, password, connect_string))
    }

    pub fn privilege(&mut self, privilege: Privilege) -> &mut Connector {
        self.0.privilege(privilege);
        self
    }

    pub fn external_auth(&mut self, on: bool) -> &mut Connector {
        self.0.external_auth(on);
        self
    }

    pub fn connect(&self) -> Result<Connection> {
        crate::wait_for_other_test_runs();
        self.0.connect().map(Connection)
    }
}
