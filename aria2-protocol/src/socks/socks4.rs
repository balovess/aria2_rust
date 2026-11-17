use super::SocksConnector;
use std::io::{Read, Write};
use std::net::SocketAddr;

/// A SOCKS4 connector configured with a user ID.
pub struct Socks4Connector {
    pub(super) user_id: String,
}

impl Socks4Connector {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
        }
    }
}

impl SocksConnector for Socks4Connector {
    fn connect<S: Read + Write>(&self, stream: S, target: &SocketAddr) -> Result<S, String> {
        super::connect_socks4(stream, target, &self.user_id)
    }
}
