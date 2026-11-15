use crate::uri::percent_decode;

/// A parsed proxy URL (for example, `socks5://user:pass@host:port`).
#[derive(Debug)]
pub struct ProxyUrl {
    pub protocol: ProxyProtocol,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
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
    /// and `https://[username:password@]host[:port]`. Any path, query, or
    /// fragment is ignored because proxy routing uses only the authority.
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        let (protocol, rest) = if let Some(rest) = url.strip_prefix("socks5://") {
            (ProxyProtocol::Socks5, rest)
        } else if let Some(rest) = url.strip_prefix("socks4://") {
            (ProxyProtocol::Socks4, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (ProxyProtocol::Http, rest)
        } else if let Some(rest) = url.strip_prefix("https://") {
            (ProxyProtocol::Https, rest)
        } else {
            return Err(format!("Unsupported proxy protocol in URL: {url}"));
        };

        let (auth_part, host_port) = match rest.find('@') {
            Some(index) => (&rest[..index], &rest[index + 1..]),
            None => ("", rest),
        };
        let host_port = host_port.split(['/', '?', '#']).next().unwrap_or(host_port);
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
                    .map_err(|_| format!("Invalid port number in proxy URL: {port_text}"))?
            } else {
                return Err(format!("Invalid bracketed proxy host: {host_port}"));
            };
            (host.to_string(), port)
        } else if let Some(index) = host_port.rfind(':') {
            let host = &host_port[..index];
            let port_text = &host_port[index + 1..];
            let port = port_text
                .parse()
                .map_err(|_| format!("Invalid port number in proxy URL: {port_text}"))?;
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
