use super::SocksConnector;
use std::io::{Read, Write};
use std::net::SocketAddr;

/// Core compatibility wrapper for the protocol crate's SOCKS4 handshake.
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
        aria2_protocol::socks::connect_socks4(stream, target, &self.user_id)
    }
}
