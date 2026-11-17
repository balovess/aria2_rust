use super::SocksConnector;
use super::socks4::Socks4Connector;
use super::socks5::Socks5Connector;
use std::io::{Read, Write};
use std::net::SocketAddr;

/// A concrete SOCKS4 or SOCKS5 connector.
pub enum SocksConnectorEnum {
    Socks4(Socks4Connector),
    Socks5(Socks5Connector),
}

impl SocksConnector for SocksConnectorEnum {
    fn connect<S: Read + Write>(&self, stream: S, target: &SocketAddr) -> Result<S, String> {
        match self {
            Self::Socks4(connector) => connector.connect(stream, target),
            Self::Socks5(connector) => connector.connect(stream, target),
        }
    }
}
