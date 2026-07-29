use super::connector_enum::SocksConnectorEnum;
use super::socks4::Socks4Connector;
use super::socks5::Socks5Connector;

/// Represents a parsed proxy URL (e.g., socks5://user:pass@host:port)
#[derive(Debug)]
pub struct ProxyUrl {
    pub protocol: ProxyProtocol,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProxyProtocol {
    Socks4,
    Socks5,
    Http,
    Https,
}

impl ProxyUrl {
    /// Parse a proxy URL string into a ProxyUrl struct.
    /// Supported formats:
    ///   socks5://[username:password@]host[:port]
    ///   socks4://[username@]host[:port]
    ///   http://[username:password@]host[:port]
    ///   https://[username:password@]host[:port]
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();

        // Determine protocol
        let (protocol, rest) = if let Some(rest) = url.strip_prefix("socks5://") {
            (ProxyProtocol::Socks5, rest)
        } else if let Some(rest) = url.strip_prefix("socks4://") {
            (ProxyProtocol::Socks4, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (ProxyProtocol::Http, rest)
        } else if let Some(rest) = url.strip_prefix("https://") {
            (ProxyProtocol::Https, rest)
        } else {
            return Err(format!("Unsupported proxy protocol in URL: {}", url));
        };

        // Split auth info from host:port
        let (auth_part, host_port) = match rest.find('@') {
            Some(idx) => (&rest[..idx], &rest[idx + 1..]),
            None => ("", rest),
        };

        // Parse username and password
        let (username, password) = if !auth_part.is_empty() {
            if let Some(colon_idx) = auth_part.find(':') {
                (
                    Some(auth_part[..colon_idx].to_string()),
                    Some(auth_part[colon_idx + 1..].to_string()),
                )
            } else {
                // SOCKS4 only uses username, no password
                (Some(auth_part.to_string()), None)
            }
        } else {
            (None, None)
        };

        // Parse host and port
        let (host, port) = if let Some(colon_idx) = host_port.rfind(':') {
            let h = &host_port[..colon_idx];
            let p_str = &host_port[colon_idx + 1..];
            let port: u16 = p_str
                .parse()
                .map_err(|_| format!("Invalid port number in proxy URL: {}", p_str))?;
            (h.to_string(), port)
        } else {
            // Use default port based on protocol
            let default_port = match protocol {
                ProxyProtocol::Socks4 | ProxyProtocol::Socks5 => 1080u16,
                ProxyProtocol::Http => 8080u16,
                ProxyProtocol::Https => 443u16,
            };
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

    /// Create the appropriate connector for this proxy URL
    pub fn create_connector(&self) -> SocksConnectorEnum {
        match self.protocol {
            ProxyProtocol::Socks4 => SocksConnectorEnum::Socks4(Socks4Connector::new(
                self.username.clone().unwrap_or_default(),
            )),
            ProxyProtocol::Socks5 => SocksConnectorEnum::Socks5(Socks5Connector::new(
                self.username.clone(),
                self.password.clone(),
            )),
            _ => panic!("HTTP/HTTPS connectors not yet implemented"),
        }
    }
}
