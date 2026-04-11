use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};

pub trait SocksConnector: Send + Sync {
    fn connect<S: Read + Write>(&self, stream: S, target: &SocketAddr) -> Result<S, String>;
}

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

/// Enum holding a concrete SOCKS4 or SOCKS5 connector (needed because SocksConnector trait has generic methods)
pub enum SocksConnectorEnum {
    Socks4(Socks4Connector),
    Socks5(Socks5Connector),
}

impl SocksConnector for SocksConnectorEnum {
    fn connect<S: Read + Write>(&self, stream: S, target: &SocketAddr) -> Result<S, String> {
        match self {
            Self::Socks4(c) => c.connect(stream, target),
            Self::Socks5(c) => c.connect(stream, target),
        }
    }
}

/// Matcher for NO_PROXY / no_proxy environment variable patterns
/// Supports patterns like:
///   - Exact domain matches: "example.com"
///   - Wildcard subdomain matches: ".example.com" (matches *.example.com)
///   - IP address exact matches: "192.168.1.1"
///   - IP/CIDR notation: "192.168.0.0/16"
///   - Special token "*": matches everything (bypass all proxies)
pub struct NoProxyMatcher {
    entries: Vec<NoProxyEntry>,
    match_all: bool,
}

enum NoProxyEntry {
    Domain(String),        // Exact domain or .domain for wildcard subdomains
    IpAddr(IpAddr),        // Exact IP address
    IpNetwork(IpAddr, u8), // IP with prefix length (CIDR)
}

impl NoProxyMatcher {
    /// Create a new NoProxyMatcher from the value of NO_PROXY/no_proxy env var.
    /// The input is typically comma-separated list of patterns.
    pub fn from_env_value(value: &str) -> Self {
        let mut entries = Vec::new();
        let mut match_all = false;

        for pattern in value.split(',') {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }

            if pattern == "*" {
                match_all = true;
                continue;
            }

            // Check for CIDR notation
            if let Some(slash_pos) = pattern.rfind('/') {
                let addr_str = &pattern[..slash_pos];
                let prefix_str = &pattern[slash_pos + 1..];
                if let Ok(addr) = addr_str.parse::<IpAddr>() {
                    if let Ok(prefix) = prefix_str.parse::<u8>() {
                        entries.push(NoProxyEntry::IpNetwork(addr, prefix));
                        continue;
                    }
                }
            }

            // Try parsing as IP address
            if let Ok(addr) = pattern.parse::<IpAddr>() {
                entries.push(NoProxyEntry::IpAddr(addr));
                continue;
            }

            // Treat as domain pattern (normalize *.prefix to .prefix for wildcard matching)
            let normalized = if pattern.starts_with("*.") {
                &pattern[1..]
            } else {
                pattern
            };
            entries.push(NoProxyEntry::Domain(normalized.to_lowercase()));
        }

        Self { entries, match_all }
    }

    /// Check if a given target address should bypass the proxy (i.e., matches NO_PROXY).
    pub fn should_bypass(&self, target: &SocketAddr) -> bool {
        if self.match_all {
            return true;
        }

        let ip = target.ip();

        for entry in &self.entries {
            match entry {
                NoProxyEntry::IpAddr(entry_ip) if *entry_ip == ip => return true,
                NoProxyEntry::IpNetwork(network_addr, prefix_len) => {
                    if Self::ip_in_network(ip, *network_addr, *prefix_len) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        false
    }

    /// Check if a hostname should bypass the proxy.
    pub fn should_bypass_hostname(&self, hostname: &str) -> bool {
        if self.match_all {
            return true;
        }

        let hostname_lower = hostname.to_lowercase();

        for entry in &self.entries {
            if let NoProxyEntry::Domain(pattern) = entry {
                if pattern.starts_with('.') {
                    // Wildcard subdomain match: .example.com matches *.example.com
                    if hostname_lower.ends_with(pattern.as_str()) || hostname_lower == &pattern[1..]
                    {
                        return true;
                    }
                } else {
                    // Exact domain match
                    if hostname_lower == *pattern {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Check if an IP falls within a CIDR network range.
    fn ip_in_network(ip: IpAddr, network: IpAddr, prefix_len: u8) -> bool {
        match (ip, network) {
            (std::net::IpAddr::V4(ip_v4), std::net::IpAddr::V4(net_v4)) => {
                let ip_u32 = u32::from_be_bytes(ip_v4.octets());
                let net_u32 = u32::from_be_bytes(net_v4.octets());
                let mask = if prefix_len >= 32 {
                    0xFFFFFFFFu32
                } else {
                    !(0xFFFFFFFF >> prefix_len)
                };
                (ip_u32 & mask) == (net_u32 & mask)
            }
            (std::net::IpAddr::V6(ip_v6), std::net::IpAddr::V6(net_v6)) => {
                let ip_octets = ip_v6.octets();
                let net_octets = net_v6.octets();
                let full_bytes = (prefix_len as usize) / 8;
                let remaining_bits = (prefix_len as usize) % 8;

                // Compare full bytes
                if ip_octets[..full_bytes] != net_octets[..full_bytes] {
                    return false;
                }

                // Compare remaining bits
                if remaining_bits > 0 && full_bytes < 16 {
                    let mask = !(0xFFu8 >> remaining_bits);
                    if (ip_octets[full_bytes] & mask) != (net_octets[full_bytes] & mask) {
                        return false;
                    }
                }

                true
            }
            _ => false,
        }
    }
}

pub struct Socks4Connector {
    pub user_id: String,
}

impl Socks4Connector {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
        }
    }
}

impl SocksConnector for Socks4Connector {
    fn connect<S: Read + Write>(&self, mut stream: S, target: &SocketAddr) -> Result<S, String> {
        let ip = match target.ip() {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => {
                return Err("SOCKS4 does not support IPv6 addresses".to_string());
            }
        };

        let port = target.port();

        let mut request = Vec::with_capacity(9 + self.user_id.len() + 1);
        request.push(0x04);
        request.push(0x01);
        request.extend_from_slice(&port.to_be_bytes());
        request.extend_from_slice(&ip.octets());
        request.extend_from_slice(self.user_id.as_bytes());
        request.push(0x00);

        stream
            .write_all(&request)
            .map_err(|e| format!("SOCKS4 failed to send request: {}", e))?;

        let mut response = [0u8; 8];
        stream
            .read_exact(&mut response)
            .map_err(|e| format!("SOCKS4 failed to read response: {}", e))?;

        if response[1] == 0x5A {
            Ok(stream)
        } else {
            let msg = match response[1] {
                0x91 => "request rejected or failed",
                0x92 => "request rejected: SOCKS server cannot connect to identd on client",
                0x93 => "request rejected: client program and identd report different user-ids",
                code => return Err(format!("SOCKS4 unknown error code: 0x{:02X}", code)),
            };
            Err(msg.to_string())
        }
    }
}

pub struct Socks5Connector {
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Socks5Connector {
    pub fn new(username: Option<String>, password: Option<String>) -> Self {
        Self { username, password }
    }

    pub fn no_auth() -> Self {
        Self {
            username: None,
            password: None,
        }
    }

    fn send_greeting<S: Read + Write>(&self, stream: &mut S) -> Result<u8, String> {
        let has_credentials = self.username.is_some() && self.password.is_some();
        let nmethods = if has_credentials { 2u8 } else { 1u8 };

        let mut greeting = vec![0x05, nmethods];
        greeting.push(0x00);
        if has_credentials {
            greeting.push(0x02);
        }

        stream
            .write_all(&greeting)
            .map_err(|e| format!("SOCKS5 failed to send greeting: {}", e))?;

        let mut reply = [0u8; 2];
        stream
            .read_exact(&mut reply)
            .map_err(|e| format!("SOCKS5 failed to read greeting response: {}", e))?;

        if reply[0] != 0x05 {
            return Err(format!(
                "SOCKS5 invalid version in greeting response: 0x{:02X}",
                reply[0]
            ));
        }

        Ok(reply[1])
    }

    fn authenticate<S: Read + Write>(&self, stream: &mut S) -> Result<(), String> {
        let username = self.username.as_deref().unwrap_or("");
        let password = self.password.as_deref().unwrap_or("");

        if username.len() > 255 || password.len() > 255 {
            return Err(
                "SOCKS5 username or password exceeds maximum length of 255 bytes".to_string(),
            );
        }

        let mut auth_req = Vec::with_capacity(3 + username.len() + password.len());
        auth_req.push(0x01);
        auth_req.push(username.len() as u8);
        auth_req.extend_from_slice(username.as_bytes());
        auth_req.push(password.len() as u8);
        auth_req.extend_from_slice(password.as_bytes());

        stream
            .write_all(&auth_req)
            .map_err(|e| format!("SOCKS5 failed to send auth request: {}", e))?;

        let mut auth_reply = [0u8; 2];
        stream
            .read_exact(&mut auth_reply)
            .map_err(|e| format!("SOCKS5 failed to read auth response: {}", e))?;

        if auth_reply[0] != 0x01 {
            return Err(format!(
                "SOCKS5 invalid auth response version: 0x{:02X}",
                auth_reply[0]
            ));
        }

        if auth_reply[1] != 0x00 {
            return Err("SOCKS5 authentication failed".to_string());
        }

        Ok(())
    }

    fn send_connect_request<S: Read + Write>(
        &self,
        stream: &mut S,
        target: &SocketAddr,
    ) -> Result<(), String> {
        let (atyp, addr_bytes) = match target.ip() {
            std::net::IpAddr::V4(v4) => (0x01u8, v4.octets().to_vec()),
            std::net::IpAddr::V6(v6) => (0x04u8, v6.octets().to_vec()),
        };

        let port = target.port();

        let mut req = Vec::with_capacity(6 + addr_bytes.len());
        req.push(0x05);
        req.push(0x01);
        req.push(0x00);
        req.push(atyp);
        req.extend_from_slice(&addr_bytes);
        req.extend_from_slice(&port.to_be_bytes());

        stream
            .write_all(&req)
            .map_err(|e| format!("SOCKS5 failed to send connect request: {}", e))?;

        let ver =
            read_u8(stream).map_err(|e| format!("SOCKS5 failed to read reply version: {}", e))?;
        if ver != 0x05 {
            return Err(format!("SOCKS5 invalid reply version: 0x{:02X}", ver));
        }

        let rep =
            read_u8(stream).map_err(|e| format!("SOCKS5 failed to read reply code: {}", e))?;
        if rep != 0x00 {
            let msg = match rep {
                0x01 => "general SOCKS server failure",
                0x02 => "connection not allowed by ruleset",
                0x03 => "network unreachable",
                0x04 => "host unreachable",
                0x05 => "connection refused",
                0x06 => "TTL expired",
                0x07 => "command not supported",
                0x08 => "address type not supported",
                code => return Err(format!("SOCKS5 unknown error code: 0x{:02X}", code)),
            };
            return Err(msg.to_string());
        }

        let _rsv =
            read_u8(stream).map_err(|e| format!("SOCKS5 failed to read reserved byte: {}", e))?;
        let atyp_reply =
            read_u8(stream).map_err(|e| format!("SOCKS5 failed to read address type: {}", e))?;

        match atyp_reply {
            0x01 => {
                let mut _bnd_addr = [0u8; 4];
                stream
                    .read_exact(&mut _bnd_addr)
                    .map_err(|e| format!("SOCKS5 failed to read bound IPv4 address: {}", e))?;
            }
            0x03 => {
                let len = read_u8(stream)
                    .map_err(|e| format!("SOCKS5 failed to read domain length: {}", e))?;
                let mut _bnd_domain = vec![0u8; len as usize];
                stream
                    .read_exact(&mut _bnd_domain)
                    .map_err(|e| format!("SOCKS5 failed to read bound domain: {}", e))?;
            }
            0x04 => {
                let mut _bnd_addr = [0u8; 16];
                stream
                    .read_exact(&mut _bnd_addr)
                    .map_err(|e| format!("SOCKS5 failed to read bound IPv6 address: {}", e))?;
            }
            _ => {
                return Err(format!(
                    "SOCKS5 unsupported address type in reply: 0x{:02X}",
                    atyp_reply
                ));
            }
        }

        let mut _bnd_port = [0u8; 2];
        stream
            .read_exact(&mut _bnd_port)
            .map_err(|e| format!("SOCKS5 failed to read bound port: {}", e))?;

        Ok(())
    }
}

impl SocksConnector for Socks5Connector {
    fn connect<S: Read + Write>(&self, mut stream: S, target: &SocketAddr) -> Result<S, String> {
        let method = self.send_greeting(&mut stream)?;

        match method {
            0x00 => {}
            0x02 => {
                self.authenticate(&mut stream)?;
            }
            _ => {
                return Err(format!(
                    "SOCKS5 server returned unacceptable authentication method: 0x{:02X}",
                    method
                ));
            }
        }

        self.send_connect_request(&mut stream, target)?;

        Ok(stream)
    }
}

fn read_u8<S: Read>(stream: &mut S) -> Result<u8, std::io::Error> {
    let mut buf = [0u8; 1];
    stream.read_exact(&mut buf)?;
    Ok(buf[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[derive(Debug)]
    struct MockTcpStream {
        reader: Cursor<Vec<u8>>,
        writer: Vec<u8>,
    }

    impl MockTcpStream {
        fn new(read_data: Vec<u8>) -> Self {
            Self {
                reader: Cursor::new(read_data),
                writer: Vec::new(),
            }
        }

        fn into_write(self) -> Vec<u8> {
            self.writer
        }
    }

    impl Read for MockTcpStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reader.read(buf)
        }
    }

    impl Write for MockTcpStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writer.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_socks4_happy_path() {
        let connector = Socks4Connector::new("testuser");
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080).into();

        let mock_response: Vec<u8> = vec![0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_ok(), "SOCKS4 happy path should succeed");

        let written = result.unwrap().into_write();
        assert_eq!(written[0], 0x04, "version byte should be 0x04");
        assert_eq!(written[1], 0x01, "command byte should be 0x01 (connect)");
        assert_eq!(&written[4..8], &[127, 0, 0, 1], "IP should be 127.0.0.1");
        assert_eq!(
            &written[8..17],
            b"testuser\0",
            "user ID should be null-terminated"
        );
    }

    #[test]
    fn test_socks4_rejected_error() {
        let connector = Socks4Connector::new("user");
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1234).into();

        let mock_response: Vec<u8> = vec![0x00, 0x91, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_err(), "SOCKS4 rejection should return error");
        let err_msg = result.err().unwrap();
        assert!(
            err_msg.contains("rejected"),
            "error message should mention rejection: {}",
            err_msg
        );
    }

    #[test]
    fn test_socks4_identd_error() {
        let connector = Socks4Connector::new("user");
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1234).into();

        let mock_response: Vec<u8> = vec![0x00, 0x92, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_err());
        let err_msg = result.err().unwrap();
        assert!(
            err_msg.contains("identd"),
            "error message should mention identd: {}",
            err_msg
        );
    }

    #[test]
    fn test_socks4_userid_mismatch_error() {
        let connector = Socks4Connector::new("user");
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1234).into();

        let mock_response: Vec<u8> = vec![0x00, 0x93, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_err());
        let err_msg = result.err().unwrap();
        assert!(
            err_msg.contains("identd") && err_msg.contains("different"),
            "error message should mention identd/user-id mismatch: {}",
            err_msg
        );
    }

    #[test]
    fn test_socks4_empty_user_id() {
        let connector = Socks4Connector::new("");
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 443).into();

        let mock_response: Vec<u8> = vec![0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_ok(), "empty user ID should work");
        let written = result.unwrap().into_write();
        assert_eq!(
            &written[4..9],
            &[192, 168, 1, 1, 0x00],
            "null terminator after IP when no user ID"
        );
    }

    #[test]
    fn test_socks5_no_auth_happy_path() {
        let connector = Socks5Connector::no_auth();
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(93, 184, 216, 34), 443).into();

        let mock_response: Vec<u8> = vec![
            0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x1F, 0x90,
        ];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_ok(), "SOCKS5 no-auth happy path should succeed");

        let written = result.unwrap().into_write();
        assert_eq!(written[0], 0x05, "greeting version should be 0x05");
        assert_eq!(written[1], 0x01, "should offer exactly 1 method (no-auth)");
        assert_eq!(written[2], 0x00, "method offered should be 0x00 (no-auth)");
        let conn_offset = 3;
        assert_eq!(written[conn_offset], 0x05, "connect request version");
        assert_eq!(written[conn_offset + 1], 0x01, "connect command");
        assert_eq!(written[conn_offset + 2], 0x00, "reserved");
        assert_eq!(written[conn_offset + 3], 0x01, "ATYP IPv4");
        assert_eq!(
            &written[conn_offset + 4..conn_offset + 8],
            &[93, 184, 216, 34],
            "target IP"
        );
        assert_eq!(
            &written[conn_offset + 8..conn_offset + 10],
            &0x01BBu16.to_be_bytes(),
            "target port 443"
        );
    }

    #[test]
    fn test_socks5_username_password_auth_happy_path() {
        let connector =
            Socks5Connector::new(Some("myuser".to_string()), Some("mypass".to_string()));
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 20, 30, 40), 8080).into();

        let mock_response: Vec<u8> = vec![
            0x05, 0x02, 0x01, 0x00, 0x05, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x1F, 0x90,
        ];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(
            result.is_ok(),
            "SOCKS5 username/password auth should succeed"
        );

        let written = result.unwrap().into_write();
        assert_eq!(written[0], 0x05, "greeting version");
        assert_eq!(written[1], 0x02, "should offer 2 methods");
        assert_eq!(written[2], 0x00, "no-auth method");
        assert_eq!(written[3], 0x02, "username/password method");
        let auth_offset = 4;
        assert_eq!(written[auth_offset], 0x01, "auth sub-negotiation version");
        assert_eq!(written[auth_offset + 1], 6, "username length");
        assert_eq!(
            &written[auth_offset + 2..auth_offset + 8],
            b"myuser",
            "username"
        );
        assert_eq!(written[auth_offset + 8], 6, "password length");
        assert_eq!(
            &written[auth_offset + 9..auth_offset + 15],
            b"mypass",
            "password"
        );
    }

    #[test]
    fn test_socks5_auth_failure() {
        let connector = Socks5Connector::new(Some("bad".to_string()), Some("cred".to_string()));
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 5678).into();

        let mock_response: Vec<u8> = vec![0x05, 0x02, 0x01, 0x01];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_err(), "auth failure should be an error");
        let err_msg = result.err().unwrap();
        assert!(
            err_msg.contains("authentication failed") || err_msg.contains("failed"),
            "error should mention auth failure: {}",
            err_msg
        );
    }

    #[test]
    fn test_socks5_connection_refused() {
        let connector = Socks5Connector::no_auth();
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53).into();

        let mock_response: Vec<u8> = vec![
            0x05, 0x00, 0x05, 0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_err(), "connection refused should be an error");
        let err_msg = result.err().unwrap();
        assert!(
            err_msg.contains("refused"),
            "error should mention connection refused: {}",
            err_msg
        );
    }

    #[test]
    fn test_socks5_unacceptable_method() {
        let connector = Socks5Connector::no_auth();
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 80).into();

        let mock_response: Vec<u8> = vec![0x05, 0xFF];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_err(), "unacceptable method should be an error");
        let err_msg = result.err().unwrap();
        assert!(
            err_msg.contains("unacceptable") || err_msg.contains("0xFF"),
            "error should mention unacceptable method: {}",
            err_msg
        );
    }

    #[test]
    fn test_socks5_general_failure() {
        let connector = Socks5Connector::no_auth();
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 1).into();

        let mock_response: Vec<u8> = vec![
            0x05, 0x00, 0x05, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_err(), "general failure should be an error");
        let err_msg = result.err().unwrap();
        assert!(
            err_msg.contains("general") || err_msg.contains("failure"),
            "error should mention general failure: {}",
            err_msg
        );
    }

    // ==================== E7: New Proxy Tests ====================

    // Test 1: SOCKS4 connect success (valid response bytes -> Ok)
    #[test]
    fn e7_test_socks4_connect_success() {
        let connector = Socks4Connector::new("proxyuser");
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(172, 16, 0, 1), 443).into();

        let mock_response: Vec<u8> = vec![0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(
            result.is_ok(),
            "SOCKS4 connect with valid response should succeed"
        );

        let written = result.unwrap().into_write();
        assert_eq!(written[0], 0x04, "SOCKS version byte");
        assert_eq!(written[1], 0x01, "CONNECT command");
    }

    // Test 2: SOCKS4 connect fail (error code 0x91 -> Err)
    #[test]
    fn e7_test_socks4_connect_fail_rejected() {
        let connector = Socks4Connector::new("test");
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 100), 80).into();

        let mock_response: Vec<u8> = vec![0x00, 0x91, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_err(), "SOCKS4 error 0x91 should return Err");
        assert!(
            result.unwrap_err().contains("rejected"),
            "error message must contain 'rejected'"
        );
    }

    // Test 3: SOCKS5 no-auth connect (Greeting 0x00 + Connect success 0x00)
    #[test]
    fn e7_test_socks5_no_auth_connect() {
        let connector = Socks5Connector::no_auth();
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 8080).into();

        let mock_response: Vec<u8> = vec![
            0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0x0a, 0x00, 0x00, 0x01, 0x1f, 0x90,
        ];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(result.is_ok(), "SOCKS5 no-auth connect should succeed");

        let written = result.unwrap().into_write();
        assert_eq!(written[0], 0x05, "greeting version");
        assert_eq!(written[1], 0x01, "one method offered");
        assert_eq!(written[2], 0x00, "no-auth method");
    }

    // Test 4: SOCKS5 password auth (Greeting 0x02 + Auth success + Connect success)
    #[test]
    fn e7_test_socks5_password_auth_connect() {
        let connector =
            Socks5Connector::new(Some("admin".to_string()), Some("secret123".to_string()));
        let target: SocketAddr = SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 9090).into();

        let mock_response: Vec<u8> = vec![
            0x05, 0x02, 0x01, 0x00, 0x05, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x23, 0x82,
        ];
        let mock_stream = MockTcpStream::new(mock_response);

        let result = connector.connect(mock_stream, &target);
        assert!(
            result.is_ok(),
            "SOCKS5 password auth connect should succeed"
        );

        let written = result.unwrap().into_write();
        assert_eq!(written[0], 0x05, "greeting version");
        assert_eq!(written[1], 0x02, "two methods offered");
        assert_eq!(written[3], 0x02, "username/password method offered");

        let auth_offset = 4;
        assert_eq!(written[auth_offset], 0x01, "auth sub-version");
        assert_eq!(written[auth_offset + 1], 5, "username length 'admin'");
        assert_eq!(
            &written[auth_offset + 2..auth_offset + 7],
            b"admin",
            "username bytes"
        );
        assert_eq!(written[auth_offset + 7], 9, "password length 'secret123'");
        assert_eq!(
            &written[auth_offset + 8..auth_offset + 17],
            b"secret123",
            "password bytes"
        );
    }

    // Test 5: No-proxy bypass matcher
    #[test]
    fn e7_test_no_proxy_bypass_matcher() {
        let matcher = NoProxyMatcher::from_env_value("*.local,localhost,example.com,.internal");

        // Should bypass: wildcard *.local matches api.local
        assert!(
            matcher.should_bypass_hostname("api.local"),
            "*.local should match api.local"
        );

        // Should NOT bypass: example.com is not in the list (example.org is different)
        assert!(
            !matcher.should_bypass_hostname("example.org"),
            "example.org should not bypass"
        );

        // Should bypass: exact match localhost
        assert!(
            matcher.should_bypass_hostname("localhost"),
            "exact match localhost should bypass"
        );

        // Should bypass: exact match example.com
        assert!(
            matcher.should_bypass_hostname("example.com"),
            "exact match example.com should bypass"
        );

        // Should bypass: .internal wildcard matches sub.internal
        assert!(
            matcher.should_bypass_hostname("sub.internal"),
            ".internal should match sub.internal"
        );

        // Should bypass: .internal matches internal itself
        assert!(
            matcher.should_bypass_hostname("internal"),
            ".internal should also match bare domain"
        );

        // Should NOT bypass: random external host
        assert!(
            !matcher.should_bypass_hostname("google.com"),
            "google.com should not bypass"
        );
    }

    // Test 5b: No-proxy IP-based matching
    #[test]
    fn e7_test_no_proxy_ip_matching() {
        use std::net::{IpAddr, Ipv4Addr};

        let matcher = NoProxyMatcher::from_env_value("192.168.1.1,10.0.0.0/8");

        let addr_v4_192: SocketAddr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 80);
        assert!(
            matcher.should_bypass(&addr_v4_192),
            "exact IP 192.168.1.1 should bypass"
        );

        let addr_v4_10: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 5, 3, 1)), 8080);
        assert!(
            matcher.should_bypass(&addr_v4_10),
            "10.5.3.1 should be within 10.0.0.0/8"
        );

        let addr_v4_external: SocketAddr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)), 443);
        assert!(
            !matcher.should_bypass(&addr_v4_external),
            "172.16.0.1 should NOT be within 10.0.0.0/8"
        );
    }

    // Test 5c: No-proxy wildcard * matches everything
    #[test]
    fn e7_test_no_proxy_wildcard_all() {
        let matcher = NoProxyMatcher::from_env_value("*");

        assert!(
            matcher.should_bypass_hostname("anything"),
            "* should bypass any hostname"
        );
        assert!(
            matcher.should_bypass(&"127.0.0.1:80".parse::<SocketAddr>().unwrap()),
            "* should bypass any IP address"
        );
    }

    // Test 6: Proxy URL parsing
    #[test]
    fn e7_test_proxy_url_parsing_socks5_with_credentials() {
        let url = "socks5://user:pass@127.0.0.1:1080";
        let parsed = ProxyUrl::parse(url).expect("should parse socks5 URL");

        assert_eq!(parsed.protocol, ProxyProtocol::Socks5);
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 1080);
        assert_eq!(parsed.username, Some("user".to_string()));
        assert_eq!(parsed.password, Some("pass".to_string()));
    }

    // Test 6b: Parse socks4 URL without credentials
    #[test]
    fn e7_test_proxy_url_parsing_socks4_no_credentials() {
        let parsed =
            ProxyUrl::parse("socks4://proxy.example.com:1080").expect("should parse socks4 URL");

        assert_eq!(parsed.protocol, ProxyProtocol::Socks4);
        assert_eq!(parsed.host, "proxy.example.com");
        assert_eq!(parsed.port, 1080);
        assert!(parsed.username.is_none());
        assert!(parsed.password.is_none());
    }

    // Test 6c: Parse HTTP proxy URL
    #[test]
    fn e7_test_proxy_url_parsing_http() {
        let parsed = ProxyUrl::parse("http://admin:secret@proxy.corp.com:3128")
            .expect("should parse http proxy URL");

        assert_eq!(parsed.protocol, ProxyProtocol::Http);
        assert_eq!(parsed.host, "proxy.corp.com");
        assert_eq!(parsed.port, 3128);
        assert_eq!(parsed.username, Some("admin".to_string()));
        assert_eq!(parsed.password, Some("secret".to_string()));
    }

    // Test 6d: Default port when omitted
    #[test]
    fn e7_test_proxy_url_default_port() {
        let socks5_parsed = ProxyUrl::parse("socks5://proxy.local").expect("should parse");
        assert_eq!(socks5_parsed.port, 1080, "SOCKS default port is 1080");

        let http_parsed = ProxyUrl::parse("http://webproxy.local").expect("should parse");
        assert_eq!(http_parsed.port, 8080, "HTTP default port is 8080");

        let https_parsed = ProxyUrl::parse("https://secure.local").expect("should parse");
        assert_eq!(https_parsed.port, 443, "HTTPS default port is 443");
    }

    // Test 6e: Invalid protocol returns error
    #[test]
    fn e7_test_proxy_url_invalid_protocol() {
        let result = ProxyUrl::parse("ftp://host:21");
        assert!(result.is_err(), "unsupported protocol should return error");
        assert!(
            result.unwrap_err().contains("Unsupported"),
            "error should mention unsupported protocol"
        );
    }

    // Test 6f: Create connector from parsed URL
    #[test]
    fn e7_test_create_connector_from_url() {
        let url = ProxyUrl::parse("socks5://myuser:mypass@10.0.0.1:9050").expect("should parse");
        let _connector = url.create_connector();
    }
}
