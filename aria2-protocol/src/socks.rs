//! SOCKS4 and SOCKS5 connection handshakes over an injected byte stream.

mod connector_enum;
mod proxy_url;
mod socks4;
mod socks5;

pub use crate::proxy::{ProxyProtocol, ProxyUrl};
pub use connector_enum::SocksConnectorEnum;
pub use socks4::Socks4Connector;
pub use socks5::Socks5Connector;

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};

/// Protocol connector interface for a SOCKS CONNECT handshake.
pub trait SocksConnector: Send + Sync {
    fn connect<S: Read + Write>(&self, stream: S, target: &SocketAddr) -> Result<S, String>;
}

/// Perform a SOCKS4 CONNECT handshake and return the tunneled stream.
pub fn connect_socks4<S: Read + Write>(
    mut stream: S,
    target: &SocketAddr,
    user_id: &str,
) -> Result<S, String> {
    if user_id.as_bytes().contains(&0) {
        return Err("SOCKS4 user ID must not contain a NUL byte".to_string());
    }
    let ip = match target.ip() {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(_) => return Err("SOCKS4 does not support IPv6 addresses".to_string()),
    };
    let mut request = Vec::with_capacity(9 + user_id.len());
    request.extend_from_slice(&[0x04, 0x01]);
    request.extend_from_slice(&target.port().to_be_bytes());
    request.extend_from_slice(&ip.octets());
    request.extend_from_slice(user_id.as_bytes());
    request.push(0x00);
    stream
        .write_all(&request)
        .map_err(|error| format!("SOCKS4 failed to send request: {error}"))?;

    let mut response = [0u8; 8];
    stream
        .read_exact(&mut response)
        .map_err(|error| format!("SOCKS4 failed to read response: {error}"))?;
    if response[0] != 0x00 {
        return Err(format!(
            "SOCKS4 invalid response version: 0x{:02X}",
            response[0]
        ));
    }
    if response[1] == 0x5A {
        return Ok(stream);
    }
    let message = match response[1] {
        0x91 => "request rejected or failed",
        0x92 => "request rejected: SOCKS server cannot connect to identd on client",
        0x93 => "request rejected: client program and identd report different user-ids",
        code => return Err(format!("SOCKS4 unknown error code: 0x{code:02X}")),
    };
    Err(message.to_string())
}

/// Perform a SOCKS5 CONNECT handshake and return the tunneled stream.
pub fn connect_socks5<S: Read + Write>(
    mut stream: S,
    target: &SocketAddr,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<S, String> {
    let credentials = match (username, password) {
        (None, None) => None,
        (Some(username), Some(password)) => {
            if username.is_empty() || password.is_empty() {
                return Err("SOCKS5 username and password must not be empty".to_string());
            }
            if username.len() > 255 || password.len() > 255 {
                return Err(
                    "SOCKS5 username or password exceeds maximum length of 255 bytes".to_string(),
                );
            }
            Some((username, password))
        }
        _ => return Err("SOCKS5 username and password must be provided together".to_string()),
    };
    let has_credentials = credentials.is_some();
    let mut greeting = vec![0x05, if has_credentials { 2 } else { 1 }, 0x00];
    if has_credentials {
        greeting.push(0x02);
    }
    stream
        .write_all(&greeting)
        .map_err(|error| format!("SOCKS5 failed to send greeting: {error}"))?;
    let mut reply = [0u8; 2];
    stream
        .read_exact(&mut reply)
        .map_err(|error| format!("SOCKS5 failed to read greeting response: {error}"))?;
    if reply[0] != 0x05 {
        return Err(format!(
            "SOCKS5 invalid version in greeting response: 0x{:02X}",
            reply[0]
        ));
    }
    match reply[1] {
        0x00 => {}
        0x02 if let Some((username, password)) = credentials => {
            authenticate(&mut stream, username, password)?
        }
        0x02 => {
            return Err(
                "SOCKS5 server selected username/password authentication that was not offered"
                    .to_string(),
            );
        }
        method => {
            return Err(format!(
                "SOCKS5 server returned unacceptable authentication method: 0x{method:02X}"
            ));
        }
    }
    send_connect_request(&mut stream, target)?;
    Ok(stream)
}

fn authenticate<S: Read + Write>(
    stream: &mut S,
    username: &str,
    password: &str,
) -> Result<(), String> {
    let mut request = Vec::with_capacity(3 + username.len() + password.len());
    request.extend_from_slice(&[0x01, username.len() as u8]);
    request.extend_from_slice(username.as_bytes());
    request.push(password.len() as u8);
    request.extend_from_slice(password.as_bytes());
    stream
        .write_all(&request)
        .map_err(|error| format!("SOCKS5 failed to send auth request: {error}"))?;
    let mut response = [0u8; 2];
    stream
        .read_exact(&mut response)
        .map_err(|error| format!("SOCKS5 failed to read auth response: {error}"))?;
    if response[0] != 0x01 {
        return Err(format!(
            "SOCKS5 invalid auth response version: 0x{:02X}",
            response[0]
        ));
    }
    if response[1] != 0x00 {
        return Err("SOCKS5 authentication failed".to_string());
    }
    Ok(())
}

fn send_connect_request<S: Read + Write>(
    stream: &mut S,
    target: &SocketAddr,
) -> Result<(), String> {
    let (address_type, address) = match target.ip() {
        IpAddr::V4(ip) => (0x01, ip.octets().to_vec()),
        IpAddr::V6(ip) => (0x04, ip.octets().to_vec()),
    };
    let mut request = Vec::with_capacity(6 + address.len());
    request.extend_from_slice(&[0x05, 0x01, 0x00, address_type]);
    request.extend_from_slice(&address);
    request.extend_from_slice(&target.port().to_be_bytes());
    stream
        .write_all(&request)
        .map_err(|error| format!("SOCKS5 failed to send connect request: {error}"))?;

    let version =
        read_u8(stream).map_err(|error| format!("SOCKS5 failed to read reply version: {error}"))?;
    if version != 0x05 {
        return Err(format!("SOCKS5 invalid reply version: 0x{version:02X}"));
    }
    let reply =
        read_u8(stream).map_err(|error| format!("SOCKS5 failed to read reply code: {error}"))?;
    if reply != 0x00 {
        let message = match reply {
            0x01 => "general SOCKS server failure",
            0x02 => "connection not allowed by ruleset",
            0x03 => "network unreachable",
            0x04 => "host unreachable",
            0x05 => "connection refused",
            0x06 => "TTL expired",
            0x07 => "command not supported",
            0x08 => "address type not supported",
            code => return Err(format!("SOCKS5 unknown error code: 0x{code:02X}")),
        };
        return Err(message.to_string());
    }
    let reserved =
        read_u8(stream).map_err(|error| format!("SOCKS5 failed to read reserved byte: {error}"))?;
    if reserved != 0x00 {
        return Err(format!(
            "SOCKS5 invalid reserved byte in reply: 0x{reserved:02X}"
        ));
    }
    let address_type =
        read_u8(stream).map_err(|error| format!("SOCKS5 failed to read address type: {error}"))?;
    match address_type {
        0x01 => discard(stream, 4, "bound IPv4 address")?,
        0x03 => {
            let length = read_u8(stream)
                .map_err(|error| format!("SOCKS5 failed to read domain length: {error}"))?;
            discard(stream, usize::from(length), "bound domain")?;
        }
        0x04 => discard(stream, 16, "bound IPv6 address")?,
        atyp => {
            return Err(format!(
                "SOCKS5 unsupported address type in reply: 0x{atyp:02X}"
            ));
        }
    }
    discard(stream, 2, "bound port")
}

fn discard<S: Read>(stream: &mut S, length: usize, field: &str) -> Result<(), String> {
    let mut bytes = vec![0u8; length];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| format!("SOCKS5 failed to read {field}: {error}"))
}

fn read_u8<S: Read>(stream: &mut S) -> std::io::Result<u8> {
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte)?;
    Ok(byte[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::{Ipv4Addr, SocketAddrV4};

    struct MockStream {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl MockStream {
        fn new(input: &[u8]) -> Self {
            Self {
                input: Cursor::new(input.to_vec()),
                output: Vec::new(),
            }
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buffer)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn legacy_socks_proxy_url_path_uses_shared_proxy_parser() {
        let proxy = ProxyUrl::parse("socks5://user%40name:secret@proxy.example:1080").unwrap();

        assert_eq!(proxy.protocol, ProxyProtocol::Socks5);
        assert_eq!(proxy.host, "proxy.example");
        assert_eq!(proxy.username.as_deref(), Some("user@name"));
        assert_eq!(proxy.password.as_deref(), Some("secret"));
    }

    #[test]
    fn socks4_encodes_connect_request_and_accepts_success() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080).into();
        let stream = MockStream::new(&[0, 0x5a, 0, 0, 0, 0, 0, 0]);
        let stream = connect_socks4(stream, &target, "u").unwrap();
        assert_eq!(stream.output, [4, 1, 0x1f, 0x90, 127, 0, 0, 1, b'u', 0]);
    }

    #[test]
    fn socks4_rejects_invalid_response_version() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080).into();
        let stream = MockStream::new(&[4, 0x5a, 0, 0, 0, 0, 0, 0]);

        assert!(matches!(
            connect_socks4(stream, &target, "u"),
            Err(error) if error.contains("invalid response version")
        ));
    }

    #[test]
    fn socks4_rejects_nul_in_user_id() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080).into();
        let stream = MockStream::new(&[]);

        assert!(matches!(
            connect_socks4(stream, &target, "user\0suffix"),
            Err(error) if error.contains("NUL byte")
        ));
    }

    #[test]
    fn socks5_authenticates_and_encodes_ipv4_connect_request() {
        let target = SocketAddrV4::new(Ipv4Addr::new(10, 20, 30, 40), 8080).into();
        let stream = MockStream::new(&[
            5, 2, // select username/password auth
            1, 0, // auth success
            5, 0, 0, 1, 0, 0, 0, 0, 0x1f, 0x90, // connect success
        ]);
        let stream = connect_socks5(stream, &target, Some("u"), Some("p")).unwrap();
        assert_eq!(
            stream.output,
            [
                5, 2, 0, 2, 1, 1, b'u', 1, b'p', 5, 1, 0, 1, 10, 20, 30, 40, 0x1f, 0x90
            ]
        );
    }

    #[test]
    fn socks5_rejects_authentication_method_that_was_not_offered() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080).into();
        let stream = MockStream::new(&[
            5, 2, // invalidly select username/password when it was not offered
            1, 0, // would accept the empty credentials sent by the old behavior
            5, 0, 0, 1, 0, 0, 0, 0, 0, 0, // successful CONNECT reply
        ]);

        let result = connect_socks5(stream, &target, None, None);

        assert!(matches!(result, Err(error) if error.contains("was not offered")));
    }

    #[test]
    fn socks5_rejects_nonzero_reserved_reply_byte() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080).into();
        let stream = MockStream::new(&[5, 0, 5, 0, 1, 1, 0, 0, 0, 0, 0, 0]);

        assert!(matches!(
            connect_socks5(stream, &target, None, None),
            Err(error) if error.contains("invalid reserved byte")
        ));
    }

    #[test]
    fn socks5_rejects_incomplete_credentials_before_writing() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080).into();
        let stream = MockStream::new(&[]);

        assert!(matches!(
            connect_socks5(stream, &target, Some("user"), None),
            Err(error) if error.contains("provided together")
        ));
    }

    #[test]
    fn socks5_rejects_empty_credentials_before_writing() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080).into();
        let stream = MockStream::new(&[]);

        assert!(matches!(
            connect_socks5(stream, &target, Some("user"), Some("")),
            Err(error) if error.contains("must not be empty")
        ));
    }

    #[test]
    fn socks5_rejects_overlong_credentials_before_writing() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080).into();
        let username = "u".repeat(256);
        let stream = MockStream::new(&[]);

        assert!(matches!(
            connect_socks5(stream, &target, Some(&username), Some("p")),
            Err(error) if error.contains("maximum length of 255 bytes")
        ));

        let password = "p".repeat(256);
        let stream = MockStream::new(&[]);
        assert!(matches!(
            connect_socks5(stream, &target, Some("u"), Some(&password)),
            Err(error) if error.contains("maximum length of 255 bytes")
        ));
    }
}
