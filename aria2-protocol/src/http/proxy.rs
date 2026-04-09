use std::env;
use tracing::{debug, info};

#[derive(Debug, Clone, PartialEq)]
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

    pub fn to_url(&self) -> String {
        match (&self.username, &self.password) {
            (Some(user), Some(pass)) => match self.proxy_type {
                ProxyType::Http => format!("http://{}:{}@{}:{}", user, pass, self.host, self.port),
                ProxyType::Socks5 => {
                    format!("socks5://{}:{}@{}:{}", user, pass, self.host, self.port)
                }
            },
            _ => match self.proxy_type {
                ProxyType::Http => format!("http://{}:{}", self.host, self.port),
                ProxyType::Socks5 => format!("socks5://{}:{}", self.host, self.port),
            },
        }
    }

    pub fn from_env() -> Option<Self> {
        if let Ok(http_proxy) = env::var("HTTP_PROXY") {
            return Self::from_url(&http_proxy);
        }
        if let Ok(http_proxy) = env::var("http_proxy") {
            return Self::from_url(&http_proxy);
        }
        if let Ok(https_proxy) = env::var("HTTPS_PROXY") {
            return Self::from_url(&https_proxy);
        }
        if let Ok(https_proxy) = env::var("https_proxy") {
            return Self::from_url(&https_proxy);
        }
        if let Ok(all_proxy) = env::var("ALL_PROXY") {
            return Self::from_url(&all_proxy);
        }
        if let Ok(all_proxy) = env::var("all_proxy") {
            return Self::from_url(&all_proxy);
        }
        None
    }

    fn from_url(url: &str) -> Option<Self> {
        let url = url.trim();

        let (proxy_type, rest) = if url.starts_with("socks5://") {
            (ProxyType::Socks5, &url[9..])
        } else if url.starts_with("http://") {
            (ProxyType::Http, &url[7..])
        } else if url.starts_with("https://") {
            (ProxyType::Http, &url[8..])
        } else {
            (ProxyType::Http, url)
        };

        let auth_split_pos = rest.find('@');
        let (auth_part, addr_part) = if let Some(pos) = auth_split_pos {
            (Some(&rest[..pos]), &rest[pos + 1..])
        } else {
            (None, rest)
        };

        let (username, password) = if let Some(auth) = auth_part {
            if let Some(colon_pos) = auth.find(':') {
                (
                    Some(auth[..colon_pos].to_string()),
                    Some(auth[colon_pos + 1..].to_string()),
                )
            } else {
                (Some(auth.to_string()), None)
            }
        } else {
            (None, None)
        };

        let colon_pos = addr_part.rfind(':')?;
        let host = &addr_part[..colon_pos];
        let port: u16 = addr_part[colon_pos + 1..].parse().ok()?;

        let config = match proxy_type {
            ProxyType::Http => Self::new_http(host, port),
            ProxyType::Socks5 => Self::new_socks5(host, port),
        };

        if let (Some(user), Some(pass)) = (username, password) {
            Some(config.with_auth(&user, &pass))
        } else {
            Some(config)
        }
    }

    pub fn should_bypass(&self, hostname: &str) -> bool {
        if let Ok(no_proxy) = env::var("NO_PROXY") {
            for pattern in no_proxy.split(',') {
                let pattern = pattern.trim();
                if pattern.is_empty() {
                    continue;
                }
                if pattern == "*" {
                    return true;
                }
                if pattern.starts_with('.') {
                    if hostname.ends_with(pattern) || hostname == &pattern[1..] {
                        return true;
                    }
                } else if hostname.eq_ignore_ascii_case(pattern) {
                    return true;
                }
            }
        }
        if let Ok(no_proxy) = env::var("no_proxy") {
            for pattern in no_proxy.split(',') {
                let pattern = pattern.trim();
                if pattern.is_empty() {
                    continue;
                }
                if pattern == "*" {
                    return true;
                }
                if pattern.starts_with('.') {
                    if hostname.ends_with(pattern) || hostname == &pattern[1..] {
                        return true;
                    }
                } else if hostname.eq_ignore_ascii_case(pattern) {
                    return true;
                }
            }
        }
        false
    }
}

pub struct ProxyManager {
    config: Option<ProxyConfig>,
}

impl ProxyManager {
    pub fn new(config: Option<ProxyConfig>) -> Self {
        if let Some(ref cfg) = config {
            info!(
                "代理配置已启用: {}:{} ({:?})",
                cfg.host, cfg.port, cfg.proxy_type
            );
        }
        Self { config }
    }

    pub fn from_env() -> Self {
        let config = ProxyConfig::from_env();
        Self::new(config)
    }

    pub fn get_proxy_for_url(&self, url: &str) -> Option<String> {
        let config = self.config.as_ref()?;
        if let Some(hostname) = Self::extract_hostname(url) {
            if config.should_bypass(&hostname) {
                debug!("跳过代理 (NO_PROXY规则匹配): {}", hostname);
                return None;
            }
        }
        Some(config.to_url())
    }

    pub fn config(&self) -> Option<&ProxyConfig> {
        self.config.as_ref()
    }

    fn extract_hostname(url: &str) -> Option<String> {
        let url = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .or_else(|| url.strip_prefix("ftp://"))?;
        let end_pos = url.find('/')?;
        Some(url[..end_pos].to_string())
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
    }

    #[test]
    fn test_should_bypass() {
        let config = ProxyConfig::new_http("proxy.example.com", 8080);
        assert!(!config.should_bypass("example.com"));
    }
}
