mod connector_enum;
mod no_proxy;
mod proxy_url;
mod socks4;
mod socks5;

#[cfg(test)]
mod tests;

pub use connector_enum::SocksConnectorEnum;
pub use no_proxy::NoProxyMatcher;
pub use proxy_url::{ProxyProtocol, ProxyUrl};
pub use socks4::Socks4Connector;
pub use socks5::Socks5Connector;

use std::io::{Read, Write};
use std::net::SocketAddr;

/// Trait for SOCKS proxy connectors.
pub trait SocksConnector: Send + Sync {
    fn connect<S: Read + Write>(&self, stream: S, target: &SocketAddr) -> Result<S, String>;
}
