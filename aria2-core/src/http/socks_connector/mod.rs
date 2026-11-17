#[cfg(test)]
mod tests;

pub use aria2_protocol::http::proxy::NoProxyMatcher;
pub use aria2_protocol::proxy::{ProxyProtocol, ProxyUrl};
pub use aria2_protocol::socks::{
    Socks4Connector, Socks5Connector, SocksConnector, SocksConnectorEnum,
};
