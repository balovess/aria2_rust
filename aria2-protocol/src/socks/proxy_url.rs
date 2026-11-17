use super::{Socks4Connector, Socks5Connector, SocksConnectorEnum};
use crate::proxy::{ProxyProtocol, ProxyUrl};

impl ProxyUrl {
    /// Create the matching connector for SOCKS URLs.
    ///
    /// Panics when called for an HTTP or HTTPS proxy URL.
    pub fn create_connector(&self) -> SocksConnectorEnum {
        match self.protocol {
            ProxyProtocol::Socks4 => SocksConnectorEnum::Socks4(Socks4Connector::new(
                self.username.clone().unwrap_or_default(),
            )),
            ProxyProtocol::Socks5 => SocksConnectorEnum::Socks5(Socks5Connector::new(
                self.username.clone(),
                self.password.clone(),
            )),
            ProxyProtocol::Http | ProxyProtocol::Https => {
                panic!("HTTP/HTTPS connectors not yet implemented")
            }
        }
    }
}
