//! Configuration for HTTP proxy connections.

use std::time::Duration;

use crate::error::{Aria2Error, Result};
use crate::http::socks_connector::ProxyUrl;

/// Configuration for an HTTP proxy connection.
///
/// Can be constructed directly or parsed from a proxy URL string via
/// [HttpProxyConfig::from_proxy_url].
#[derive(Debug, Clone)]
pub struct HttpProxyConfig {
    /// Proxy server hostname or IP address
    pub proxy_host: String,
    /// Proxy server port
    pub proxy_port: u16,
    /// Optional proxy username for authentication
    pub proxy_username: Option<String>,
    /// Optional proxy password for authentication
    pub proxy_password: Option<String>,
    /// Target server hostname we want to reach through the proxy
    pub target_host: String,
    /// Target server port we want to reach through the proxy
    pub target_port: u16,
    /// Timeout for establishing the TCP connection to the proxy
    pub connect_timeout: Duration,
    /// Timeout for reading data from the proxy
    pub read_timeout: Duration,
    /// Timeout for writing data to the proxy
    pub write_timeout: Duration,
}

impl HttpProxyConfig {
    /// Create a new proxy config with default timeouts (30s connect, 60s read/write).
    pub fn new(proxy_host: String, proxy_port: u16, target_host: String, target_port: u16) -> Self {
        Self {
            proxy_host,
            proxy_port,
            proxy_username: None,
            proxy_password: None,
            target_host,
            target_port,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(60),
        }
    }

    /// Set proxy authentication credentials.
    pub fn with_credentials(mut self, username: String, password: String) -> Self {
        self.proxy_username = Some(username);
        self.proxy_password = Some(password);
        self
    }

    /// Parse a proxy URL string (e.g., http://user:pass@host:port) into a config.
    ///
    /// Uses the existing [ProxyUrl] parser from the socks_connector module.
    /// Only HTTP and HTTPS proxy protocols are supported; SOCKS URLs return an error.
    pub fn from_proxy_url(
        proxy_url: &str,
        target_host: String,
        target_port: u16,
    ) -> Result<Self> {
        let parsed = ProxyUrl::parse(proxy_url).map_err(|e| {
            Aria2Error::Parse(format!("Invalid proxy URL '{}': {}", proxy_url, e))
        })?;

        match parsed.protocol {
            crate::http::socks_connector::ProxyProtocol::Http
            | crate::http::socks_connector::ProxyProtocol::Https => {}
            _ => {
                return Err(Aria2Error::Parse(format!(
                    "Expected http/https proxy URL, got: {:?}",
                    parsed.protocol
                )));
            }
        }

        let mut config = Self::new(parsed.host, parsed.port, target_host, target_port);

        if let Some(user) = parsed.username {
            config.proxy_username = Some(user);
        }
        if let Some(pass) = parsed.password {
            config.proxy_password = Some(pass);
        }

        Ok(config)
    }

    /// The host:port string for the target (used in CONNECT and Host headers).
    pub(crate) fn target_host_port(&self) -> String {
        format!("{}:{}", self.target_host, self.target_port)
    }
}
