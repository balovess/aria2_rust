use tokio::io::BufReader;
use tokio::net::TcpStream;
use tracing::{debug, warn};

use super::{FtpActiveDataListener, FtpConnection, FtpResponse};

impl FtpConnection {
    pub async fn pasv(&mut self) -> Result<(String, u16), String> {
        debug!("Requesting passive mode data connection");
        self.send_command("PASV").await?;
        let resp = self.read_response().await?;
        if resp.code != 227 {
            return Err(format!("PASV failed: {} {}", resp.code, resp.message));
        }

        match Self::parse_pasv_response(&resp.message) {
            Some(addr) => {
                debug!("PASV data channel: {}:{}", addr.0, addr.1);
                Ok(addr)
            }
            None => Err("Failed to parse PASV response".to_string()),
        }
    }

    pub async fn epsv(&mut self) -> Result<u16, String> {
        debug!("Requesting extended passive mode");
        self.send_command("EPSV").await?;
        let resp = self.read_response().await?;
        if resp.code == 229 {
            if let Some(port) = Self::parse_epsv_response(&resp.message) {
                debug!("EPSV port: {}", port);
                return Ok(port);
            }
        } else if resp.code == 590 || resp.code == 500 || resp.code == 501 {
            warn!("Server does not support EPSV, falling back to PASV mode");
            let (_, port) = self.pasv().await?;
            return Ok(port);
        }
        Err(format!("EPSV failed: {} {}", resp.code, resp.message))
    }

    /// Prepare active mode using the IPv4 `PORT` command.
    pub async fn prepare_port_active(&mut self) -> Result<FtpActiveDataListener, String> {
        debug!("Requesting active mode data connection (IPv4)");
        let control_addr = self
            .stream
            .get_ref()
            .local_addr()
            .map_err(|e| format!("Failed to get control local address: {}", e))?;
        let ip = control_addr.ip();
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::new(ip, 0))
            .await
            .map_err(|e| format!("Failed to bind local port: {}", e))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))?;
        let port = local_addr.port();
        let octets = match ip {
            std::net::IpAddr::V4(v4) => v4.octets(),
            std::net::IpAddr::V6(_) => {
                return Err(
                    "IPv6 address not supported for PORT command, use EPRT instead".to_string(),
                );
            }
        };
        let p1 = port / 256;
        let p2 = port % 256;
        let port_cmd = format!(
            "PORT {},{},{},{},{},{}",
            octets[0], octets[1], octets[2], octets[3], p1, p2
        );

        debug!("Sending PORT command: {}", port_cmd);
        self.send_command(&port_cmd).await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("PORT failed: {} {}", resp.code, resp.message));
        }

        debug!("Waiting for active mode data connection on port {}", port);
        Ok(FtpActiveDataListener::new(listener, local_addr))
    }

    /// Enter active mode using PORT command (IPv4).
    ///
    /// This legacy method returns only the port. Call
    /// [`Self::prepare_port_active`] when the listener must remain alive until
    /// the server opens the data connection.
    pub async fn port_active(&mut self) -> Result<u16, String> {
        let listener = self.prepare_port_active().await?;
        Ok(listener.port())
    }

    /// Prepare active mode using the extended `EPRT` command.
    pub async fn prepare_eprt_active(&mut self) -> Result<FtpActiveDataListener, String> {
        debug!("Requesting extended active mode data connection");
        let control_addr = self
            .stream
            .get_ref()
            .local_addr()
            .map_err(|e| format!("Failed to get control local address: {}", e))?;
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::new(control_addr.ip(), 0))
                .await
                .map_err(|e| format!("Failed to bind local port: {}", e))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))?;
        let ip = local_addr.ip();
        let port = local_addr.port();
        let (proto, addr_str) = match ip {
            std::net::IpAddr::V4(v4) => ("1", v4.to_string()),
            std::net::IpAddr::V6(v6) => ("2", v6.to_string()),
        };
        let eprt_cmd = format!("EPRT |{}|{}|{}", proto, addr_str, port);

        debug!("Sending EPRT command: {}", eprt_cmd);
        self.send_command(&eprt_cmd).await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("EPRT failed: {} {}", resp.code, resp.message));
        }

        debug!("EPRT successful, listening {}:{}", addr_str, port);
        Ok(FtpActiveDataListener::new(listener, local_addr))
    }

    /// Enter active mode using EPRT and return the advertised endpoint.
    ///
    /// This legacy method returns only the endpoint. Call
    /// [`Self::prepare_eprt_active`] when the listener must remain alive until
    /// the server opens the data connection.
    pub async fn eprt_active(&mut self) -> Result<(String, u16), String> {
        let listener = self.prepare_eprt_active().await?;
        let addr = listener.local_addr();
        Ok((addr.ip().to_string(), addr.port()))
    }

    /// Send LIST command to get directory listing (detailed format).
    pub async fn list(&mut self, path: Option<&str>) -> Result<FtpResponse, String> {
        match path {
            Some(p) => {
                debug!("Listing directory details: {}", p);
                self.send_command(&format!("LIST {}", p)).await?
            }
            None => {
                debug!("Listing current directory details");
                self.send_command("LIST").await?
            }
        };
        self.read_response().await
    }

    /// Send NLST command to get directory listing (names only).
    pub async fn nlst(&mut self, path: Option<&str>) -> Result<FtpResponse, String> {
        match path {
            Some(p) => {
                debug!("Listing directory names: {}", p);
                self.send_command(&format!("NLST {}", p)).await?
            }
            None => {
                debug!("Listing current directory names");
                self.send_command("NLST").await?
            }
        };
        self.read_response().await
    }

    pub fn get_data_stream(&mut self) -> &mut BufReader<TcpStream> {
        &mut self.stream
    }

    pub(crate) fn parse_pasv_response(message: &str) -> Option<(String, u16)> {
        let start = message.find('(')?;
        let end = message.find(')')?;
        let inner = &message[start + 1..end];
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() != 6 {
            return None;
        }

        let h1: u8 = parts[0].trim().parse().ok()?;
        let h2: u8 = parts[1].trim().parse().ok()?;
        let h3: u8 = parts[2].trim().parse().ok()?;
        let h4: u8 = parts[3].trim().parse().ok()?;
        let p1: u16 = parts[4].trim().parse().ok()?;
        let p2: u16 = parts[5].trim().parse().ok()?;

        Some((format!("{}.{}.{}.{}", h1, h2, h3, h4), p1 * 256 + p2))
    }

    pub(crate) fn parse_epsv_response(message: &str) -> Option<u16> {
        let start = message.rfind('|')?;
        let prev_pipe = message[..start].rfind('|')?;
        message[prev_pipe + 1..start].parse::<u16>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn active_data_listener_accepts_connection_after_preparation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind active data listener");
        let local_addr = listener.local_addr().expect("read listener address");
        let active = FtpActiveDataListener::new(listener, local_addr);

        let connector = tokio::spawn(async move {
            tokio::net::TcpStream::connect(local_addr)
                .await
                .expect("connect to active data listener")
        });
        let accepted = tokio::time::timeout(std::time::Duration::from_secs(1), active.accept())
            .await
            .expect("active data connection should not time out")
            .expect("accept active data connection");

        assert_eq!(
            accepted.peer_addr().expect("read peer address").ip(),
            local_addr.ip()
        );
        connector.await.expect("connector task should finish");
    }
}
