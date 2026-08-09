//! Raw FTP control channel handler and PASV/EPSV response parsers.
//!
//! Contains the `RawFtpControl` struct for managing FTP command/response
//! interactions, plus helper functions for parsing passive mode responses
//! and URL-encoded strings.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use crate::ftp::connection::{self, FtpControlStream, FtpDataStream, FtpsConfig};
use crate::network::ConnectionContext;

use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

// ---------------------------------------------------------------------------
// URL decoding
// ---------------------------------------------------------------------------

/// URL-encoded string decoder
pub(crate) fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push(c);
                result.push_str(&hex);
            }
        } else {
            result.push(c);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// RawFtpControl
// ---------------------------------------------------------------------------

/// Raw FTP control connection handler
pub(super) struct RawFtpControl {
    reader: BufReader<FtpControlStream>,
    host: String,
    connection: ConnectionContext,
    ftps_config: Option<FtpsConfig>,
}

impl RawFtpControl {
    async fn connect_tcp_at(
        host: &str,
        port: u16,
        socket_addr: std::net::SocketAddr,
    ) -> Result<(tokio::net::TcpStream, ConnectionContext)> {
        let addr = format!("{}:{}", host, port);
        debug!("Connecting to FTP server at {} via {}", addr, socket_addr);

        let stream = tokio::net::TcpStream::connect(socket_addr)
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP connect failed to {}:{}: {}", host, port, e),
                })
            })?;
        let peer_addr = stream.peer_addr().map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP peer address unavailable: {}", e),
            })
        })?;
        let connection = ConnectionContext::new(host, port, peer_addr);

        stream.set_nodelay(true).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {}", e),
            })
        })?;

        Ok((stream, connection))
    }

    fn from_stream(
        stream: FtpControlStream,
        host: &str,
        connection: ConnectionContext,
        ftps_config: Option<FtpsConfig>,
    ) -> Self {
        Self {
            reader: BufReader::new(stream),
            host: host.to_string(),
            connection,
            ftps_config,
        }
    }

    async fn read_welcome(&mut self) -> Result<()> {
        let welcome = self
            .read_response(Duration::from_secs(constants::FTP_WELCOME_TIMEOUT_SECS))
            .await?;

        if !(200..300).contains(&welcome.0) && !(100..200).contains(&welcome.0) {
            return Err(Aria2Error::Fatal(FatalError::Config(format!(
                "FTP server rejected connection: {} {}",
                welcome.0, welcome.1
            ))));
        }

        Ok(())
    }

    pub(super) async fn connect_at(
        host: &str,
        port: u16,
        socket_addr: std::net::SocketAddr,
    ) -> Result<Self> {
        let (stream, connection) = Self::connect_tcp_at(host, port, socket_addr).await?;
        let mut ctrl = Self::from_stream(FtpControlStream::Plain(stream), host, connection, None);
        ctrl.read_welcome().await?;

        info!("Connected to FTP server {}:{}", host, port);
        Ok(ctrl)
    }

    /// Connect to an explicit FTPS endpoint and perform RFC 4217 setup.
    pub(super) async fn connect_ftps_explicit_at(
        host: &str,
        port: u16,
        socket_addr: std::net::SocketAddr,
        config: &FtpsConfig,
    ) -> Result<Self> {
        let (stream, connection) = Self::connect_tcp_at(host, port, socket_addr).await?;
        let mut plain = Self::from_stream(FtpControlStream::Plain(stream), host, connection, None);
        plain.read_welcome().await?;

        let Self {
            reader,
            host,
            connection,
            ..
        } = plain;
        let stream = match reader.into_inner() {
            FtpControlStream::Plain(stream) => stream,
            FtpControlStream::Tls(_) => unreachable!("fresh FTPS control stream is plain"),
        };
        let tls_stream = connection::upgrade_control_stream(stream, &host, config)
            .await
            .map_err(|error| {
                Aria2Error::Network(format!("FTPS control upgrade failed: {}", error))
            })?;

        info!("FTPS control connection established with {}:{}", host, port);
        Ok(Self::from_stream(
            FtpControlStream::Tls(Box::new(tls_stream)),
            &host,
            connection,
            Some(config.clone()),
        ))
    }

    /// Connect to an implicit FTPS endpoint where TLS starts immediately.
    pub(super) async fn connect_ftps_implicit_at(
        host: &str,
        port: u16,
        socket_addr: std::net::SocketAddr,
        config: &FtpsConfig,
    ) -> Result<Self> {
        let (stream, connection) = Self::connect_tcp_at(host, port, socket_addr).await?;
        let tls_stream = connection::upgrade_data_stream(stream, host, config)
            .await
            .map_err(|error| {
                Aria2Error::Network(format!("FTPS TLS handshake failed: {}", error))
            })?;
        let mut ctrl = Self::from_stream(
            FtpControlStream::Tls(Box::new(tls_stream)),
            host,
            connection,
            Some(config.clone()),
        );
        ctrl.read_welcome().await?;

        info!(
            "Implicit FTPS control connection established with {}:{}",
            host, port
        );
        Ok(ctrl)
    }

    /// Send a command to the FTP server
    pub(super) async fn send_command(&mut self, cmd: &str) -> Result<()> {
        debug!("FTP CMD: {}", cmd.trim());
        self.reader
            .get_mut()
            .write_all(format!("{}\r\n", cmd).as_bytes())
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP write command failed: {}", e),
                })
            })?;
        self.reader.get_mut().flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP flush failed: {}", e),
            })
        })?;
        Ok(())
    }

    /// Read response from FTP server with timeout
    pub(super) async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();
            let bytes_read = tokio::time::timeout(timeout_dur, self.reader.read_line(&mut line))
                .await
                .map_err(|_| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP response timeout after {:?}", timeout_dur),
                    })
                })?
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP read response error: {}", e),
                    })
                })?;

            if bytes_read == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                continue;
            }

            let response_code: u16 = trimmed[..3].parse().unwrap_or(0);
            if code.is_none() {
                code = Some(response_code);
            }

            let sep = trimmed.as_bytes()[3];
            if sep == b'-' && !is_multiline {
                is_multiline = true;
                if trimmed.len() > 4 {
                    message.push_str(&trimmed[4..]);
                }
                message.push('\n');
                continue;
            }
            if is_multiline {
                if trimmed.starts_with(&format!("{} ", code.unwrap_or(0))) {
                    if trimmed.len() > 4 {
                        message.push_str(&trimmed[4..]);
                    }
                    break;
                }
                if trimmed.len() > 4 {
                    message.push_str(&trimmed[4..]);
                }
                message.push('\n');
                continue;
            }

            if trimmed.len() > 4 {
                message = trimmed[4..].to_string();
            }
            break;
        }

        let code_val = code.unwrap_or(0);
        debug!("FTP RESP: {} {}", code_val, message.trim());
        Ok((code_val, message))
    }

    pub(super) fn connection_context(&self) -> &ConnectionContext {
        &self.connection
    }

    pub(super) async fn secure_data_stream(
        &self,
        stream: tokio::net::TcpStream,
    ) -> Result<FtpDataStream> {
        if let Some(config) = &self.ftps_config {
            let tls_stream = connection::upgrade_data_stream(stream, &self.host, config)
                .await
                .map_err(|error| {
                    Aria2Error::Network(format!("FTPS data TLS handshake failed: {}", error))
                })?;
            Ok(FtpDataStream::Tls(Box::new(tls_stream)))
        } else {
            Ok(FtpDataStream::Plain(stream))
        }
    }

    /// Send command and read response in one operation
    pub(super) async fn command(&mut self, cmd: &str) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(Duration::from_secs(constants::FTP_COMMAND_TIMEOUT_SECS))
            .await
    }

    /// Authenticate with USER/PASS commands
    pub(super) async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        info!("Authenticating as user: {}", username);

        let user_resp = self.command(&format!("USER {}", username)).await?;
        match user_resp.0 {
            230 => {
                // Login successful without password
                info!("FTP login successful (no password required)");
                Ok(())
            }
            331 | 332 => {
                // Password required
                debug!("Password required, sending PASS command");
                let pass_resp = self.command(&format!("PASS {}", password)).await?;
                if !(200..300).contains(&pass_resp.0) {
                    return Err(Aria2Error::Fatal(FatalError::PermissionDenied {
                        path: format!("Login failed: {} {}", pass_resp.0, pass_resp.1),
                    }));
                }
                info!("FTP login successful");
                Ok(())
            }
            _ => Err(Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("Unexpected USER response: {} {}", user_resp.0, user_resp.1),
            })),
        }
    }

    /// Set binary transfer mode (TYPE I)
    pub(super) async fn set_binary_mode(&mut self) -> Result<()> {
        debug!("Setting transfer mode to binary (TYPE I)");
        let resp = self.command("TYPE I").await?;
        if !(200..300).contains(&resp.0) {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("TYPE I failed: {} {}", resp.0, resp.1),
                },
            ));
        }
        Ok(())
    }

    /// Set resume offset (REST command)
    pub(super) async fn set_resume_offset(&mut self, offset: u64) -> Result<bool> {
        if offset == 0 {
            return Ok(true);
        }
        debug!("Setting resume offset: {} bytes", offset);
        let resp = self.command(&format!("REST {}", offset)).await?;
        if resp.0 != 350 {
            warn!("REST command not accepted by server: {} {}", resp.0, resp.1);
            // Some servers do not support REST. Report this to the caller so
            // it can restart from byte zero instead of appending at a stale
            // local offset while the server sends the complete object.
            return Ok(false);
        }
        Ok(true)
    }

    /// Get file size (SIZE command)
    pub(super) async fn get_file_size(&mut self, remote_path: &str) -> Result<Option<u64>> {
        debug!("Querying file size: {}", remote_path);
        let resp = self.command(&format!("SIZE {}", remote_path)).await?;
        if resp.0 == 213 {
            let size = resp.1.trim().parse::<u64>().map_err(|error| {
                Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
                    message: format!("Invalid FTP SIZE response {:?}: {}", resp.1, error),
                })
            })?;
            debug!("File size: {} bytes", size);
            return Ok(Some(size));
        }
        if resp.0 == 550 {
            return Err(Aria2Error::Fatal(FatalError::FileNotFound {
                path: remote_path.to_string(),
            }));
        }
        // SIZE command may not be supported by all servers
        debug!("SIZE command returned: {} {}", resp.0, resp.1);
        Ok(None)
    }

    /// Enter passive mode (PASV/EPSV)
    pub(super) async fn enter_passive_mode(&mut self) -> Result<(String, u16)> {
        // Try EPSV first (supports IPv6), fallback to PASV
        debug!("Attempting extended passive mode (EPSV)");
        let epsv_resp = self.command("EPSV").await;

        match epsv_resp {
            Ok(resp) if resp.0 == 229 => {
                // Parse |||port| format
                if let Some(port) = parse_epsv_response(&resp.1) {
                    debug!("EPSV successful, using port: {}", port);
                    return Ok((self.host.clone(), port));
                }
                warn!("Failed to parse EPSV response, falling back to PASV");
            }
            _ => {
                debug!("EPSV not supported, trying PASV");
            }
        }

        // Fallback to PASV
        debug!("Entering passive mode (PASV)");
        let pasv_resp = self.command("PASV").await?;
        if pasv_resp.0 != 227 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("PASV failed: {} {}", pasv_resp.0, pasv_resp.1),
                },
            ));
        }

        match parse_pasv_response(&pasv_resp.1) {
            Some((host, port)) => {
                debug!("PASV successful, data channel: {}:{}", host, port);
                Ok((host, port))
            }
            None => Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "Cannot parse PASV response".into(),
                },
            )),
        }
    }

    /// Create an active-mode listener and advertise it with EPRT/PORT.
    pub(super) async fn enter_active_mode(&mut self) -> Result<tokio::net::TcpListener> {
        let local_addr = self
            .reader
            .get_ref()
            .get_ref()
            .ok_or_else(|| Aria2Error::Network("FTP local address unavailable".into()))?
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("FTP local address unavailable: {}", e)))?;
        let listener = tokio::net::TcpListener::bind(match local_addr {
            std::net::SocketAddr::V4(_) => "0.0.0.0:0",
            std::net::SocketAddr::V6(_) => "[::]:0",
        })
        .await
        .map_err(|e| Aria2Error::Network(format!("FTP active listener bind failed: {}", e)))?;
        let port = listener
            .local_addr()
            .map_err(|e| {
                Aria2Error::Network(format!("FTP active listener address unavailable: {}", e))
            })?
            .port();
        let ip = local_addr.ip();
        let eprt = format!(
            "EPRT |{}|{}|{}|",
            if ip.is_ipv4() { 1 } else { 2 },
            ip,
            port
        );
        let response = self.command(&eprt).await?;
        if !(200..300).contains(&response.0) {
            if let std::net::IpAddr::V4(ipv4) = ip {
                let octets = ipv4.octets();
                let port_cmd = format!(
                    "PORT {},{},{},{},{},{}",
                    octets[0],
                    octets[1],
                    octets[2],
                    octets[3],
                    port / 256,
                    port % 256
                );
                let port_response = self.command(&port_cmd).await?;
                if !(200..300).contains(&port_response.0) {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!(
                                "PORT failed: {} {}",
                                port_response.0, port_response.1
                            ),
                        },
                    ));
                }
            } else {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("EPRT failed for IPv6: {} {}", response.0, response.1),
                    },
                ));
            }
        }
        Ok(listener)
    }

    /// Initiate file retrieval (RETR command)
    pub(super) async fn initiate_retr(&mut self, remote_path: &str) -> Result<()> {
        debug!("Initiating file retrieval: {}", remote_path);
        let resp = self.command(&format!("RETR {}", remote_path)).await?;
        if resp.0 != 150 && resp.0 != 125 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("RETR unexpected response: {} {}", resp.0, resp.1),
                },
            ));
        }
        Ok(())
    }

    /// Read final transfer completion response
    pub(super) async fn read_transfer_complete(&mut self) -> Result<()> {
        match self
            .read_response(Duration::from_secs(
                constants::FTP_TRANSFER_COMPLETE_TIMEOUT_SECS,
            ))
            .await
        {
            Ok((226, msg)) => {
                debug!("Transfer complete: {}", msg);
                Ok(())
            }
            Ok((code, msg)) => {
                warn!("Transfer response non-226: {} {}", code, msg);
                // Some servers don't send 226 properly, but data was received OK
                Ok(())
            }
            Err(e) => {
                debug!("Transfer completion timeout/error (may be normal): {}", e);
                Ok(())
            }
        }
    }

    /// Gracefully disconnect from server
    pub(super) async fn quit(mut self) -> Result<()> {
        debug!("Sending QUIT command");
        let _ = self.command("QUIT").await.ok(); // Ignore errors on quit
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PASV / EPSV response parsers
// ---------------------------------------------------------------------------

/// Parse PASV response to extract IP and port
pub(super) fn parse_pasv_response(response: &str) -> Option<(String, u16)> {
    let start = response.find('(')?;
    let end = response.rfind(')')?;
    let inner = &response[start + 1..end];
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

/// Parse EPSV response to extract port
pub(super) fn parse_epsv_response(response: &str) -> Option<u16> {
    let start = response.rfind('|')?;
    let prev_pipe = response[..start].rfind('|')?;
    let port_str = &response[prev_pipe + 1..start];
    port_str.parse::<u16>().ok()
}
