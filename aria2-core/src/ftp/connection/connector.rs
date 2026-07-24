//! FTP connection establishment and mode configuration
//!
//! Contains methods for connecting to an FTP server, logging in,
//! setting transfer mode, and establishing passive/active data connections.

use crate::error::{Aria2Error, Result};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use super::types::{FtpClient, FtpMode};

impl FtpClient {
    /// Default connection timeout: 30 seconds
    const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    /// Default read timeout: 30 seconds
    const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

    /// Connect to an FTP server
    ///
    /// # Arguments
    ///
    /// - `host`: FTP server address (domain name or IP)
    /// - `port`: FTP server port (typically 21)
    /// - `mode`: Data connection mode (passive or active)
    ///
    /// # Errors
    ///
    /// - Connection timeout
    /// - Network error
    /// - Server refused connection
    pub async fn connect(host: &str, port: u16, mode: FtpMode) -> Result<Self> {
        info!("FTP connecting: {}:{}", host, port);

        let stream = timeout(
            Self::DEFAULT_CONNECT_TIMEOUT,
            TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("FTP connection failed: {}", e)))?;

        let mut client = Self {
            control_stream: tokio::io::BufReader::new(stream),
            mode,
            binary_mode: false,
            host: host.to_string(),
            port,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
            read_timeout: Self::DEFAULT_READ_TIMEOUT,
            base_working_dir: "/".to_string(),
        };

        // Read welcome message
        let welcome = client.read_response().await?;
        if !welcome.is_positive_completion() && !welcome.is_positive_preliminary() {
            return Err(Aria2Error::DownloadFailed(format!(
                "FTP server refused connection: {} {}",
                welcome.code, welcome.message
            )));
        }

        debug!("FTP connected successfully: {}", welcome.message.trim());
        Ok(client)
    }

    /// Log in to the FTP server
    ///
    /// # Arguments
    ///
    /// - `username`: Username (use "anonymous" for anonymous login)
    /// - `password`: Password (use email for anonymous login)
    ///
    /// # Errors
    ///
    /// - 530 Not logged in (authentication failed)
    pub async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        debug!("Sending USER command: {}", username);
        self.send_command(&format!("USER {}", username)).await?;
        let resp = self.read_response().await?;

        match resp.code {
            230 => {
                // No password required, login successful
                info!("FTP login successful (no password required)");
                Ok(())
            }
            331 | 332 => {
                // Password required
                debug!("Password authentication required, sending PASS command");
                self.send_command(&format!("PASS {}", password)).await?;
                let pass_resp = self.read_response().await?;

                if pass_resp.code == 230 || pass_resp.code == 202 {
                    info!("FTP login successful");
                    Ok(())
                } else if pass_resp.code == 530 {
                    Err(Aria2Error::Recoverable(
                        crate::error::RecoverableError::ServerError { code: 530 },
                    ))
                } else {
                    Err(Aria2Error::DownloadFailed(format!(
                        "FTP login failed: {} {}",
                        pass_resp.code, pass_resp.message
                    )))
                }
            }
            530 => Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 530 },
            )),
            _ => {
                if resp.is_positive_completion() {
                    info!("FTP login successful");
                    Ok(())
                } else {
                    Err(Aria2Error::DownloadFailed(format!(
                        "FTP login failed: {} {}",
                        resp.code, resp.message
                    )))
                }
            }
        }
    }

    /// Set transfer mode (binary/ASCII)
    ///
    /// # Arguments
    ///
    /// - `enabled`: true for binary mode (TYPE I), false for ASCII mode (TYPE A)
    ///
    /// # Errors
    ///
    /// - 504 Unsupported transfer mode
    pub async fn set_binary_mode(&mut self, enabled: bool) -> Result<()> {
        let type_cmd = if enabled { "TYPE I" } else { "TYPE A" };
        debug!("Setting transfer type: {}", type_cmd);
        self.send_command(type_cmd).await?;
        let resp = self.read_response().await?;

        if resp.is_positive_completion() {
            self.binary_mode = enabled;
            debug!(
                "Transfer mode set to: {}",
                if enabled { "Binary" } else { "ASCII" }
            );
            Ok(())
        } else if resp.code == 504 {
            Err(Aria2Error::DownloadFailed(format!(
                "Unsupported transfer mode: {}",
                resp.message
            )))
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "TYPE command failed: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// Enter passive mode and establish data connection
    ///
    /// Tries EPSV (Extended Passive Mode) first, falls back to PASV if the
    /// server does not support it.
    ///
    /// # Returns
    ///
    /// Returns the data connection TcpStream
    ///
    /// # Errors
    ///
    /// - 425 Cannot open data connection
    /// - Timeout error
    pub async fn passive_mode(&mut self) -> Result<TcpStream> {
        debug!("Requesting passive mode data connection");

        // Try EPSV first
        self.send_command("EPSV").await?;
        let resp = self.read_response().await?;

        if resp.code == 229 {
            // Parse EPSV response: Entering Extended Passive Mode (|||port|)
            if let Some(port) = Self::parse_epsv_response(&resp.message) {
                debug!("EPSV data channel port: {}", port);
                let data_stream = timeout(
                    self.connect_timeout,
                    TcpStream::connect((self.host.as_str(), port)),
                )
                .await
                .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
                .map_err(|e| Aria2Error::Network(format!("EPSV data connection failed: {}", e)))?;

                return Ok(data_stream);
            }
        }

        // Fall back to PASV
        warn!("EPSV unavailable, falling back to PASV mode");
        self.send_command("PASV").await?;
        let pasv_resp = self.read_response().await?;

        if pasv_resp.code != 227 {
            return Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 425 },
            ));
        }

        // Parse PASV response
        let (data_host, data_port) = Self::parse_pasv_response(&pasv_resp.message)?;
        debug!("PASV data channel: {}:{}", data_host, data_port);
        let data_stream = timeout(
            self.connect_timeout,
            TcpStream::connect((data_host.as_str(), data_port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("PASV data connection failed: {}", e)))?;
        Ok(data_stream)
    }

    /// Enter active mode and establish data connection
    ///
    /// Sends a PORT or EPRT command to inform the server of the client's data port,
    /// then listens on that port for the server to connect.
    ///
    /// # Returns
    ///
    /// Returns the accepted data connection TcpStream
    ///
    /// # Errors
    ///
    /// - 425 Cannot open data connection
    /// - 500/501/502 Command syntax error
    pub async fn active_mode(&mut self) -> Result<TcpStream> {
        debug!("Requesting active mode data connection");

        // Get local address
        let local_addr = self
            .control_stream
            .get_ref()
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("Failed to get local address: {}", e)))?;

        // Listen on port 0 (system auto-assigns an available port)
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to bind data port: {}", e)))?;
        let data_port = listener
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("Failed to get listen port: {}", e)))?
            .port();

        let local_ip = local_addr.ip();

        // Try EPRT (extended active mode)
        let eprt_cmd = format!("EPRT |1|{}|{}|", local_ip, data_port);
        debug!("Sending EPRT command: {}", eprt_cmd);
        self.send_command(&eprt_cmd).await?;
        let resp = self.read_response().await?;

        if resp.code != 200 && resp.code != 500 && resp.code != 501 && resp.code != 502 {
            return Err(Aria2Error::DownloadFailed(format!(
                "EPRT command failed: {} {}",
                resp.code, resp.message
            )));
        }

        // If EPRT failed, try PORT command
        if !resp.is_positive_completion() {
            warn!("EPRT unavailable, falling back to PORT mode");

            // Convert IP address to PORT command format (h1,h2,h3,h4,p1,p2)
            // Only supports IPv4 (PORT command does not support IPv6)

            let ipv4_addr = match local_ip {
                std::net::IpAddr::V4(v4) => v4,
                std::net::IpAddr::V6(_) => {
                    // IPv6 does not support PORT command, return error
                    return Err(Aria2Error::DownloadFailed(
                        "IPv6 does not support active mode PORT command, please use passive mode"
                            .to_string(),
                    ));
                }
            };
            let ip_bytes = ipv4_addr.octets();
            let p1 = data_port / 256;
            let p2 = data_port % 256;
            let port_cmd = format!(
                "PORT {},{},{},{},{},{}",
                ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3], p1, p2
            );

            debug!("Sending PORT command: {}", port_cmd);
            self.send_command(&port_cmd).await?;
            let port_resp = self.read_response().await?;

            if !port_resp.is_positive_completion() {
                return Err(Aria2Error::Recoverable(
                    crate::error::RecoverableError::ServerError { code: 425 },
                ));
            }
        }

        // Wait for server connection (with timeout)
        debug!("Waiting for server to connect to data port: {}", data_port);
        let (data_stream, _addr) = timeout(self.connect_timeout, listener.accept())
            .await
            .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
            .map_err(|e| Aria2Error::Network(format!("Failed to accept data connection: {}", e)))?;

        debug!("Active mode data connection established successfully");
        Ok(data_stream)
    }
}
