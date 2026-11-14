#[cfg(test)]
mod tests;

pub use aria2_protocol::http::proxy::NoProxyMatcher;
pub use aria2_protocol::socks::{
    ProxyProtocol, ProxyUrl, Socks4Connector, Socks5Connector, SocksConnector, SocksConnectorEnum,
};
