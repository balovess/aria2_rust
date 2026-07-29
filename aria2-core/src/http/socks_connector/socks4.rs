use super::SocksConnector;
use std::io::{Read, Write};
use std::net::SocketAddr;

pub struct Socks4Connector {
    pub(super) user_id: String,
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
