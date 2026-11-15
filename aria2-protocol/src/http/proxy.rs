#[path = "no_proxy.rs"]
mod no_proxy;

use std::env;
use tracing::{debug, info};

use crate::proxy::{ProxyProtocol, ProxyUrl};
use crate::uri::percent_encode;

pub use no_proxy::NoProxyMatcher;

/// Classification of an HTTP response status received from a proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyResponseKind {
    /// The proxy accepted the request.
    Connected,
    /// The proxy requires authentication.
    AuthenticationRequired,
    /// The proxy returned another status code.
    Error,
}

/// Classify a proxy response status without interpreting core policy.
pub fn classify_proxy_response(status_code: u16) -> ProxyResponseKind {
    match status_code {
        200 => ProxyResponseKind::Connected,
        407 => ProxyResponseKind::AuthenticationRequired,
        _ => ProxyResponseKind::Error,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyType {
    Http,
    Socks5,
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub proxy_type: ProxyType,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyConfig {
    pub fn new_http(host: &str, port: u16) -> Self {
        Self {
            proxy_type: ProxyType::Http,
            host: host.to_string(),
            port,
            username: None,
            password: None,
        }
    }

    pub fn new_socks5(host: &str, port: u16) -> Self {
        Self {
            proxy_type: ProxyType::Socks5,
            host: host.to_string(),
            port,
            username: None,
            password: None,
        }
    }

    pub fn with_auth(mut self, username: &str, password: &str) -> Self {
        self.username = Some(username.to_string());
        self.password = Some(password.to_string());
        self
    }

    /// Serialize the proxy URL, escaping credentials and bracketing IPv6 hosts.
    pub fn to_url(&self) -> String {
        let scheme = match self.proxy_type {
            ProxyType::Http => "http",
            ProxyType::Socks5 => "socks5",
        };
        let host_value = self
            .host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(&self.host);
        let host = if host_value.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{host_value}]")
        } else {
            self.host.clone()
        };
        let auth = match (&self.username, &self.password) {
            (Some(user), Some(pass)) => {
                format!("{}:{}@", percent_encode(user), percent_encode(pass))
            }
            _ => String::new(),
        };

        format!("{scheme}://{auth}{host}:{}", self.port)
    }

    pub fn from_env() -> Option<Self> {
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            if let Ok(value) = env::var(name) {
                return Self::from_url(&value);
            }
        }
        None
    }

    fn from_url(url: &str) -> Option<Self> {
        let url = url.trim();
        let normalized_url = if url.contains("://") {
            url.to_string()
        } else {
            format!("http://{url}")
        };
        let parsed = ProxyUrl::parse(&normalized_url).ok()?;
        let config = match parsed.protocol {
            ProxyProtocol::Socks5 => Self::new_socks5(&parsed.host, parsed.port),
            // This legacy configuration type only models HTTP and SOCKS5.
            // Keep non-SOCKS5 schemes on its HTTP representation.
            ProxyProtocol::Socks4 | ProxyProtocol::Http | ProxyProtocol::Https => {
                Self::new_http(&parsed.host, parsed.port)
            }
        };

        if let (Some(user), Some(pass)) = (parsed.username, parsed.password) {
            Some(config.with_auth(&user, &pass))
        } else {
            Some(config)
        }
    }

    pub fn should_bypass(&self, hostname: &str) -> bool {
        let no_proxy = [env::var("NO_PROXY").ok(), env::var("no_proxy").ok()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(",");
        NoProxyMatcher::from_env_value(&no_proxy).should_bypass_hostname(hostname)
    }
}

pub struct ProxyManager {
    config: Option<ProxyConfig>,
}

impl ProxyManager {
    pub fn new(config: Option<ProxyConfig>) -> Self {
        if let Some(ref cfg) = config {
            info!(
                "Proxy configuration enabled: {}:{} ({:?})",
                cfg.host, cfg.port, cfg.proxy_type
            );
        }
        Self { config }
    }

    /// Load the first available proxy from the legacy environment order.
    pub fn from_env() -> Self {
        Self::new(ProxyConfig::from_env())
    }

    /// Return the configured proxy for a URL unless its hostname is in
    /// `NO_PROXY` or `no_proxy`.
    pub fn get_proxy_for_url(&self, url: &str) -> Option<String> {
        let config = self.config.as_ref()?;
        if let Some(hostname) = Self::extract_hostname(url)
            && config.should_bypass(&hostname)
        {
            debug!("Skipping proxy (NO_PROXY rule matched): {}", hostname);
            return None;
        }
        Some(config.to_url())
    }

    pub fn config(&self) -> Option<&ProxyConfig> {
        self.config.as_ref()
    }

    fn extract_hostname(url: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        if matches!(parsed.scheme(), "http" | "https" | "ftp") {
            let hostname = parsed.host_str()?;
            let hostname = hostname
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(hostname);
            Some(hostname.to_owned())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_to_url() {
        let config = ProxyConfig::new_http("proxy.example.com", 8080);
        assert_eq!(config.to_url(), "http://proxy.example.com:8080");

        let socks = ProxyConfig::new_socks5("localhost", 1080);
        assert_eq!(socks.to_url(), "socks5://localhost:1080");

        let with_auth = ProxyConfig::new_http("proxy.example.com", 8080).with_auth("user", "pass");
        assert_eq!(
            with_auth.to_url(),
            "http://user:pass@proxy.example.com:8080"
        );
    }

    #[test]
    fn proxy_url_round_trip_encodes_credentials_and_ipv6_authority() {
        let config =
            ProxyConfig::from_url("socks5://user%40name:p%3A%40%25@[2001:db8::1]:9050").unwrap();

        assert_eq!(config.username.as_deref(), Some("user@name"));
        assert_eq!(config.password.as_deref(), Some("p:@%"));
        let rendered = config.to_url();
        assert_eq!(
            rendered,
            "socks5://user%40name:p%3A%40%25@[2001:db8::1]:9050"
        );

        let parsed = ProxyUrl::parse(&rendered).unwrap();
        assert_eq!(parsed.host, "2001:db8::1");
        assert_eq!(parsed.username.as_deref(), Some("user@name"));
        assert_eq!(parsed.password.as_deref(), Some("p:@%"));
    }

    #[test]
    fn test_proxy_from_url() {
        let config = ProxyConfig::from_url("http://proxy.example.com:8080").unwrap();
        assert_eq!(config.proxy_type, ProxyType::Http);
        assert_eq!(config.host, "proxy.example.com");
        assert_eq!(config.port, 8080);

        let with_auth = ProxyConfig::from_url("http://user:pass@proxy.example.com:3128").unwrap();
        assert_eq!(with_auth.username.as_deref(), Some("user"));
        assert_eq!(with_auth.password.as_deref(), Some("pass"));

        let socks = ProxyConfig::from_url("socks5://127.0.0.1:1080").unwrap();
        assert_eq!(socks.proxy_type, ProxyType::Socks5);

        let default_port = ProxyConfig::from_url("http://proxy.example.com").unwrap();
        assert_eq!(default_port.host, "proxy.example.com");
        assert_eq!(default_port.port, 8080);

        let https = ProxyConfig::from_url("https://secure.proxy.example/path").unwrap();
        assert_eq!(https.proxy_type, ProxyType::Http);
        assert_eq!(https.port, 443);

        let bare = ProxyConfig::from_url("proxy.example.com:3128").unwrap();
        assert_eq!(bare.host, "proxy.example.com");
        assert_eq!(bare.port, 3128);

        let socks_default_port = ProxyConfig::from_url("socks5://proxy.example.com").unwrap();
        assert_eq!(socks_default_port.port, 1080);
    }

    #[test]
    fn test_should_bypass() {
        let config = ProxyConfig::new_http("proxy.example.com", 8080);
        assert!(!config.should_bypass("example.com"));
    }

    #[test]
    fn proxy_manager_extracts_hosts_from_urls_with_or_without_paths() {
        assert_eq!(
            ProxyManager::extract_hostname("http://example.test"),
            Some("example.test".to_string())
        );
        assert_eq!(
            ProxyManager::extract_hostname("https://example.test:8443/path?q=1"),
            Some("example.test".to_string())
        );
        assert_eq!(
            ProxyManager::extract_hostname("ftp://[2001:db8::1]:21/file"),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(ProxyManager::extract_hostname("file:///tmp/file"), None);
        assert_eq!(ProxyManager::extract_hostname("not a URL"), None);
    }
}
