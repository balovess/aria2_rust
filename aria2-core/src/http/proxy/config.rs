//! Configuration for HTTP proxy connections.
//!
//! Supports HTTP CONNECT tunnels, forward proxies, and SOCKS4/5 proxies.
//! All proxy types share the same config structure; the `proxy_type` field
//! determines how the connection is established.

use std::time::Duration;

use tracing::debug;

use crate::error::{Aria2Error, Result};
use crate::http::socks_connector::ProxyUrl;

/// The type of proxy being used.
///
/// Determines the wire protocol for establishing the proxied connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyType {
    /// HTTP proxy using CONNECT tunnel (for HTTPS targets) or forward relay (for HTTP targets)
    Http,
    /// HTTPS proxy (TLS to proxy, then CONNECT or forward)
    Https,
    /// SOCKS4 proxy (no password auth, user-id only)
    Socks4,
    /// SOCKS5 proxy (supports username/password auth)
    Socks5,
}

impl std::fmt::Display for ProxyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyType::Http => write!(f, "HTTP"),
            ProxyType::Https => write!(f, "HTTPS"),
            ProxyType::Socks4 => write!(f, "SOCKS4"),
            ProxyType::Socks5 => write!(f, "SOCKS5"),
        }
    }
}

/// Configuration for a proxy connection.
///
/// Can be constructed directly or parsed from a proxy URL string via
/// [HttpProxyConfig::from_proxy_url].
///
/// # Proxy Types
///
/// | Type     | URL scheme | Auth support            |
/// |----------|------------|-------------------------|
/// | HTTP     | http://    | Basic / Digest          |
/// | HTTPS    | https://   | Basic / Digest (TLS)    |
/// | SOCKS4   | socks4://  | User-id only            |
/// | SOCKS5   | socks5://  | Username/password       |
#[derive(Debug, Clone)]
pub struct HttpProxyConfig {
    /// Proxy server hostname or IP address
    pub proxy_host: String,
    /// Proxy server port
    pub proxy_port: u16,
    /// Proxy type (HTTP, HTTPS, SOCKS4, SOCKS5)
    pub proxy_type: ProxyType,
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
    /// Create a new HTTP proxy config with default timeouts (30s connect, 60s read/write).
    pub fn new(proxy_host: String, proxy_port: u16, target_host: String, target_port: u16) -> Self {
        Self {
            proxy_host,
            proxy_port,
            proxy_type: ProxyType::Http,
            proxy_username: None,
            proxy_password: None,
            target_host,
            target_port,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(60),
        }
    }

    /// Create a new proxy config with an explicit proxy type.
    pub fn with_type(
        proxy_type: ProxyType,
        proxy_host: String,
        proxy_port: u16,
        target_host: String,
        target_port: u16,
    ) -> Self {
        Self {
            proxy_host,
            proxy_port,
            proxy_type,
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

    /// Parse a proxy URL string into a config.
    ///
    /// Supports all proxy types:
    /// - `http://[user:pass@]host:port` -> HTTP proxy
    /// - `https://[user:pass@]host:port` -> HTTPS proxy
    /// - `socks4://[userid@]host:port` -> SOCKS4 proxy
    /// - `socks5://[user:pass@]host:port` -> SOCKS5 proxy
    ///
    /// Uses the existing [ProxyUrl] parser from the socks_connector module.
    pub fn from_proxy_url(
        proxy_url: &str,
        target_host: String,
        target_port: u16,
    ) -> Result<Self> {
        let parsed = ProxyUrl::parse(proxy_url).map_err(|e| {
            Aria2Error::Parse(format!("Invalid proxy URL '{}': {}", proxy_url, e))
        })?;

        let proxy_type = match parsed.protocol {
            crate::http::socks_connector::ProxyProtocol::Http => ProxyType::Http,
            crate::http::socks_connector::ProxyProtocol::Https => ProxyType::Https,
            crate::http::socks_connector::ProxyProtocol::Socks4 => ProxyType::Socks4,
            crate::http::socks_connector::ProxyProtocol::Socks5 => ProxyType::Socks5,
        };

        let mut config = Self::with_type(
            proxy_type,
            parsed.host,
            parsed.port,
            target_host,
            target_port,
        );

        if let Some(user) = parsed.username {
            config.proxy_username = Some(user);
        }
        if let Some(pass) = parsed.password {
            config.proxy_password = Some(pass);
        }

        debug!(
            "Parsed proxy URL: type={}, host={}, port={}, has_auth={}",
            config.proxy_type,
            config.proxy_host,
            config.proxy_port,
            config.proxy_username.is_some()
        );

        Ok(config)
    }

    /// Whether this is a SOCKS-type proxy.
    pub fn is_socks(&self) -> bool {
        matches!(self.proxy_type, ProxyType::Socks4 | ProxyType::Socks5)
    }

    /// Whether this is an HTTP/HTTPS proxy (CONNECT or forward).
    pub fn is_http_proxy(&self) -> bool {
        matches!(self.proxy_type, ProxyType::Http | ProxyType::Https)
    }

    /// The host:port string for the target (used in CONNECT and Host headers).
    pub(crate) fn target_host_port(&self) -> String {
        format!("{}:{}", self.target_host, self.target_port)
    }
}
