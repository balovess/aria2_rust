use crate::uri::percent_decode;

/// A parsed proxy URL (for example, `socks5://user:pass@host:port`).
pub struct ProxyUrl {
    pub protocol: ProxyProtocol,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for ProxyUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyUrl")
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username.as_ref().map(|_| "[REDACTED]"))
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// The protocol selected by a proxy URL scheme.
#[derive(Debug, Clone, PartialEq)]
pub enum ProxyProtocol {
    Socks4,
    Socks5,
    Http,
    Https,
}

impl ProxyUrl {
    /// Parse a proxy URL and percent-decode its username and password.
    ///
    /// Supported forms are `socks5://[username:password@]host[:port]`,
    /// `socks4://[username@]host[:port]`, `http://[username:password@]host[:port]`,
    /// and `https://[username:password@]host[:port]`. Schemes are matched
    /// case-insensitively. Any path, query, or fragment is ignored because
    /// proxy routing uses only the authority.
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| "Invalid proxy URL: expected scheme://authority".to_string())?;
        let protocol = if scheme.eq_ignore_ascii_case("socks5") {
            ProxyProtocol::Socks5
        } else if scheme.eq_ignore_ascii_case("socks4") {
            ProxyProtocol::Socks4
        } else if scheme.eq_ignore_ascii_case("http") {
            ProxyProtocol::Http
        } else if scheme.eq_ignore_ascii_case("https") {
            ProxyProtocol::Https
        } else {
            return Err(format!("Unsupported proxy protocol '{scheme}'"));
        };

        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let (auth_part, host_port) = match authority.find('@') {
            Some(index) => (&authority[..index], &authority[index + 1..]),
            None => ("", authority),
        };
        let (username, password) = if !auth_part.is_empty() {
            if let Some(index) = auth_part.find(':') {
                (
                    Some(percent_decode(&auth_part[..index])),
                    Some(percent_decode(&auth_part[index + 1..])),
                )
            } else {
                (Some(percent_decode(auth_part)), None)
            }
        } else {
            (None, None)
        };

        let default_port = match protocol {
            ProxyProtocol::Socks4 | ProxyProtocol::Socks5 => 1080,
            ProxyProtocol::Http => 8080,
            ProxyProtocol::Https => 443,
        };
        let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
            let close = bracketed
                .find(']')
                .ok_or_else(|| "Missing closing bracket in proxy IPv6 host".to_string())?;
            let host = &bracketed[..close];
            let suffix = &bracketed[close + 1..];
            let port = if suffix.is_empty() {
                default_port
            } else if let Some(port_text) = suffix.strip_prefix(':') {
                port_text
                    .parse()
                    .map_err(|_| "Invalid port number in proxy URL".to_string())?
            } else {
                return Err("Invalid bracketed proxy host".to_string());
            };
            (host.to_string(), port)
        } else if let Some(index) = host_port.rfind(':') {
            let host = &host_port[..index];
            let port_text = &host_port[index + 1..];
            let port = port_text
                .parse()
                .map_err(|_| "Invalid port number in proxy URL".to_string())?;
            (host.to_string(), port)
        } else {
            (host_port.to_string(), default_port)
        };

        if host.is_empty() {
            return Err("Host is empty in proxy URL".to_string());
        }

        Ok(Self {
            protocol,
            host,
            port,
            username,
            password,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ProxyProtocol, ProxyUrl};

    #[test]
    fn proxy_url_debug_redacts_credentials() {
        let proxy = ProxyUrl {
            protocol: ProxyProtocol::Http,
            host: "proxy.example".to_string(),
            port: 8080,
            username: Some("private-user".to_string()),
            password: Some("private-password".to_string()),
        };

        let debug = format!("{proxy:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("private-user"));
        assert!(!debug.contains("private-password"));
    }

    #[test]
    fn proxy_url_parses_case_insensitive_scheme_and_decodes_credentials() {
        let proxy = ProxyUrl::parse("HtTp://user%40name:p%3A%40%25@proxy.example").unwrap();

        assert_eq!(proxy.protocol, ProxyProtocol::Http);
        assert_eq!(proxy.host, "proxy.example");
        assert_eq!(proxy.port, 8080);
        assert_eq!(proxy.username.as_deref(), Some("user@name"));
        assert_eq!(proxy.password.as_deref(), Some("p:@%"));
    }

    #[test]
    fn proxy_url_ignores_userinfo_delimiters_after_authority() {
        let proxy = ProxyUrl::parse("http://proxy.example/path@marker?next=@query#@fragment")
            .expect("path, query, and fragment are ignored");

        assert_eq!(proxy.host, "proxy.example");
        assert_eq!(proxy.port, 8080);
        assert_eq!(proxy.username, None);
        assert_eq!(proxy.password, None);

        let proxy = ProxyUrl::parse("http://user:pass@proxy.example/path@marker")
            .expect("authority userinfo should still parse");
        assert_eq!(proxy.username.as_deref(), Some("user"));
        assert_eq!(proxy.password.as_deref(), Some("pass"));
        assert_eq!(proxy.host, "proxy.example");
    }

    #[test]
    fn proxy_url_parse_errors_do_not_include_userinfo() {
        let error = ProxyUrl::parse("unknown://private-user:private-password@proxy.example")
            .expect_err("unknown schemes should be rejected");

        assert!(!error.contains("private-user"));
        assert!(!error.contains("private-password"));
    }
}
