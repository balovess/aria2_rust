use super::SocksConnector;
use std::io::{Read, Write};
use std::net::SocketAddr;

pub struct Socks5Connector {
    pub(super) username: Option<String>,
    pub(super) password: Option<String>,
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
