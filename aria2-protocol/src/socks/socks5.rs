use super::SocksConnector;
use std::io::{Read, Write};
use std::net::SocketAddr;

/// A SOCKS5 connector configured with optional username/password auth.
pub struct Socks5Connector {
    pub(super) username: Option<String>,
    pub(super) password: Option<String>,
}

impl Socks5Connector {
    pub fn new(username: Option<String>, password: Option<String>) -> Self {
        Self { username, password }
    }

    pub fn no_auth() -> Self {
        Self {
            username: None,
            password: None,
        }
    }
}

impl SocksConnector for Socks5Connector {
    fn connect<S: Read + Write>(&self, stream: S, target: &SocketAddr) -> Result<S, String> {
        super::connect_socks5(
            stream,
            target,
            self.username.as_deref(),
            self.password.as_deref(),
        )
    }
}
