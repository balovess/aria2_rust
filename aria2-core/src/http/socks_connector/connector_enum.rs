use super::socks4::Socks4Connector;
use super::socks5::Socks5Connector;
use super::SocksConnector;
use std::io::{Read, Write};
use std::net::SocketAddr;

/// Enum holding a concrete SOCKS4 or SOCKS5 connector (needed because SocksConnector trait has generic methods)
pub enum SocksConnectorEnum {
    Socks4(Socks4Connector),
    Socks5(Socks5Connector),
}

impl SocksConnector for SocksConnectorEnum {
    fn connect<S: Read + Write>(&self, stream: S, target: &SocketAddr) -> Result<S, String> {
        match self {
            Self::Socks4(c) => c.connect(stream, target),
            Self::Socks5(c) => c.connect(stream, target),
        }
    }
}
