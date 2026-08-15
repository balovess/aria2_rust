//! Raw FTP control channel handler and PASV/EPSV response parsers.
//!
//! Contains the `RawFtpControl` struct for managing FTP command/response
//! interactions, plus helper functions for parsing passive mode responses
//! and URL-encoded strings.

use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use crate::ftp::connection::{
    self, FtpControlStream, FtpDataStream, FtpProxyConfig, FtpProxyTunnel, FtpProxyTunnelConfig,
    FtpsConfig, active_data_bind_addr, cwd_targets, parse_mdtm_timestamp, parse_pwd_response,
    read_response_impl, split_decoded_remote_path,
};
use crate::network::ConnectionContext;

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};

// ---------------------------------------------------------------------------
// URL decoding
// ---------------------------------------------------------------------------

/// Keep the legacy local name while routing URI decoding through the
/// canonical FTP path decoder. This preserves UTF-8 bytes across `%XX`
/// sequences instead of converting each byte directly into a `char`.
pub(crate) use crate::ftp::connection::percent_decode as urlencoding_decode;

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

        if welcome.0 != 220 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("FTP greeting rejected: {} {}", welcome.0, welcome.1),
                },
            ));
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

    /// Connect to an FTP server through an HTTP CONNECT proxy.
    ///
    /// The proxy connection is established before the FTP greeting is read,
    /// matching aria2's tunnel command chain while keeping the control state
    /// owned by this Rust command.
    pub(super) async fn connect_via_http_proxy(
        host: &str,
        port: u16,
        proxy: &FtpProxyConfig,
        ftps_config: Option<&FtpsConfig>,
        ftps_implicit: bool,
    ) -> Result<Self> {
        let tunnel_config = FtpProxyTunnelConfig {
            proxy_host: proxy.proxy_host.clone(),
            proxy_port: proxy.proxy_port,
            target_host: host.to_string(),
            target_port: port,
            proxy_username: proxy.proxy_username.clone(),
            proxy_password: proxy.proxy_password.clone(),
            connect_timeout: proxy.connect_timeout,
            read_timeout: proxy.connect_timeout,
            user_agent: proxy.user_agent.clone(),
        };
        let stream = FtpProxyTunnel::establish(&tunnel_config).await?;
        let peer_addr = stream.peer_addr().map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP proxy peer address unavailable: {}", error),
            })
        })?;
        let connection = ConnectionContext::new(host, port, peer_addr);

        if let Some(config) = ftps_config {
            if ftps_implicit {
                let tls_stream = connection::perform_tls_handshake(stream, host, config)
                    .await
                    .map_err(|error| {
                        Aria2Error::Network(format!("FTPS proxy TLS handshake failed: {}", error))
                    })?;
                let mut ctrl = Self::from_stream(
                    FtpControlStream::Tls(Box::new(tls_stream)),
                    host,
                    connection,
                    Some(config.clone()),
                );
                ctrl.read_welcome().await?;
                return Ok(ctrl);
            }

            let mut plain = Self::from_stream(
                FtpControlStream::Plain(stream),
                host,
                connection.clone(),
                None,
            );
            plain.read_welcome().await?;
            let Self {
                reader,
                host,
                connection,
                ..
            } = plain;
            let stream = match reader.into_inner() {
                FtpControlStream::Plain(stream) => stream,
                FtpControlStream::Tls(_) => unreachable!("fresh FTPS proxy stream is plain"),
            };
            let tls_stream = connection::upgrade_control_stream(stream, &host, config)
                .await
                .map_err(|error| {
                    Aria2Error::Network(format!("FTPS proxy control upgrade failed: {}", error))
                })?;
            let mut ctrl = Self::from_stream(
                FtpControlStream::Tls(Box::new(tls_stream)),
                &host,
                connection,
                Some(config.clone()),
            );
            ctrl.read_welcome().await?;
            return Ok(ctrl);
        }

        let mut ctrl = Self::from_stream(FtpControlStream::Plain(stream), host, connection, None);
        ctrl.read_welcome().await?;
        info!(
            "Connected to FTP server {}:{} through HTTP proxy {}:{}",
            host, port, proxy.proxy_host, proxy.proxy_port
        );
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
        let tls_stream = connection::perform_tls_handshake(stream, host, config)
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
        read_response_impl(&mut self.reader, timeout_dur).await
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
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::FtpProtocolError {
                            message: format!("Login failed: {} {}", pass_resp.0, pass_resp.1),
                        },
                    ));
                }
                info!("FTP login successful");
                Ok(())
            }
            _ => Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("Unexpected USER response: {} {}", user_resp.0, user_resp.1),
                },
            )),
        }
    }

    /// Set binary transfer mode (TYPE I)
    pub(super) async fn set_binary_mode(&mut self) -> Result<()> {
        debug!("Setting transfer mode to binary (TYPE I)");
        let resp = self.command("TYPE I").await?;
        if resp.0 != 200 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("TYPE I failed: {} {}", resp.0, resp.1),
                },
            ));
        }
        Ok(())
    }

    /// Select the remote directory before issuing file commands.
    ///
    /// aria2_original asks for PWD after TYPE, sends CWD for the base working
    /// directory and each URI directory component, then addresses SIZE/RETR
    /// with only the file name. The production engine owns its own async
    /// control flow, so this small adapter keeps that wire contract without
    /// importing the original state machine.
    pub(super) async fn prepare_remote_path(&mut self, remote_path: &str) -> Result<String> {
        let pwd = self.command("PWD").await?;
        if pwd.0 != 257 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("PWD failed: {} {}", pwd.0, pwd.1),
                },
            ));
        }
        let base_working_dir = parse_pwd_response(&pwd.1).ok_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
                message: format!("PWD response missing quoted path: {}", pwd.1.trim()),
            })
        })?;
        let (directory, file) = split_decoded_remote_path(remote_path);

        for target in cwd_targets(&base_working_dir, &directory) {
            let response = self.command(&format!("CWD {}", target)).await?;
            if response.0 == 550 {
                return Err(Aria2Error::Recoverable(RecoverableError::ResourceNotFound));
            }
            if response.0 != 250 {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::FtpProtocolError {
                        message: format!("CWD failed: {} {}", response.0, response.1),
                    },
                ));
            }
        }

        Ok(file)
    }

    /// Query the remote modification time after directory traversal.
    ///
    /// `aria2_original` treats MDTM as optional: a non-213 response is logged
    /// and the download continues without applying a timestamp. Network and
    /// malformed control responses still use the normal FTP error path.
    pub(super) async fn get_modification_time(
        &mut self,
        remote_path: &str,
    ) -> Result<Option<std::time::SystemTime>> {
        let response = self.command(&format!("MDTM {}", remote_path)).await?;
        if response.0 != 213 {
            debug!(
                code = response.0,
                message = %response.1,
                "FTP MDTM is unavailable for remote file"
            );
            return Ok(None);
        }

        let response_message = response.1.trim();
        let timestamp = response_message
            .strip_prefix("213")
            .unwrap_or(response_message)
            .trim()
            .get(..14)
            .and_then(parse_mdtm_timestamp);
        if timestamp.is_none() {
            warn!(response = %response.1, "FTP MDTM response has no valid timestamp");
        }
        Ok(timestamp)
    }

    /// Set resume offset (REST command)
    pub(super) async fn set_resume_offset(&mut self, offset: u64) -> Result<bool> {
        debug!("Setting resume offset: {} bytes", offset);
        let resp = self.command(&format!("REST {}", offset)).await?;
        if resp.0 != 350 {
            warn!("REST command not accepted by server: {} {}", resp.0, resp.1);
            // Some servers do not support REST. Report this to the caller so
            // it can restart from byte zero instead of appending at a stale
            // local offset while the server sends the complete object.
            return Ok(offset == 0);
        }
        Ok(true)
    }

    /// Get file size (SIZE command)
    pub(super) async fn get_file_size(&mut self, remote_path: &str) -> Result<Option<u64>> {
        debug!("Querying file size: {}", remote_path);
        let resp = self.command(&format!("SIZE {}", remote_path)).await?;
        if resp.0 == 213 {
            let size = parse_ftp_size_response(&resp.1)?;
            debug!("File size: {} bytes", size);
            return Ok(Some(size));
        }
        if resp.0 == 550 {
            return Err(Aria2Error::Recoverable(RecoverableError::ResourceNotFound));
        }
        // SIZE command may not be supported by all servers
        debug!("SIZE command returned: {} {}", resp.0, resp.1);
        Ok(None)
    }

    /// Enter passive mode (PASV/EPSV) and establish the data socket.
    ///
    /// aria2_original deliberately connects the data socket to the control
    /// connection's peer address. The host advertised in a PASV response is
    /// parsed for wire validation and diagnostics, but is not a connection
    /// target because NATed and misconfigured servers commonly advertise an
    /// unreachable address.
    pub(super) async fn enter_passive_mode(&mut self) -> Result<tokio::net::TcpStream> {
        let port;
        // Try EPSV first (supports IPv6), fallback to PASV
        debug!("Attempting extended passive mode (EPSV)");
        let epsv_resp = self.command("EPSV").await;

        match epsv_resp {
            Ok(resp) if resp.0 == 229 => {
                // Parse |||port| format
                if let Some(parsed_port) = parse_epsv_response(&resp.1) {
                    debug!("EPSV successful, using port: {}", parsed_port);
                    port = parsed_port;
                } else {
                    warn!("Failed to parse EPSV response, falling back to PASV");
                    port = self.enter_passive_mode_pasv().await?;
                }
            }
            _ => {
                debug!("EPSV not supported, trying PASV");
                port = self.enter_passive_mode_pasv().await?;
            }
        };

        let data_addr = std::net::SocketAddr::new(self.connection.peer_addr.ip(), port);
        tokio::time::timeout(
            Duration::from_secs(constants::FTP_DATA_CONNECTION_TIMEOUT_SECS),
            tokio::net::TcpStream::connect(data_addr),
        )
        .await
        .map_err(|_| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Data connection timeout via {}", data_addr),
            })
        })?
        .map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Data connection failed via {}: {}", data_addr, error),
            })
        })
    }

    async fn enter_passive_mode_pasv(&mut self) -> Result<u16> {
        debug!("Entering passive mode (PASV)");
        let pasv_resp = self.command("PASV").await?;
        if pasv_resp.0 != 227 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("PASV failed: {} {}", pasv_resp.0, pasv_resp.1),
                },
            ));
        }

        match parse_pasv_response(&pasv_resp.1) {
            Some((advertised_host, port)) => {
                debug!(
                    advertised_host,
                    control_peer = %self.connection.peer_addr,
                    port,
                    "PASV successful; using control peer address for data channel"
                );
                Ok(port)
            }
            None => Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
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
        let listener = tokio::net::TcpListener::bind(active_data_bind_addr(local_addr))
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
                        RecoverableError::FtpProtocolError {
                            message: format!(
                                "PORT failed: {} {}",
                                port_response.0, port_response.1
                            ),
                        },
                    ));
                }
            } else {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::FtpProtocolError {
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
            if resp.0 == 550 {
                return Err(Aria2Error::Recoverable(RecoverableError::ResourceNotFound));
            }
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
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

    pub(super) async fn abort_transfer(&mut self) {
        let _ = self.command("ABOR").await;
    }

    /// Gracefully disconnect from server
    pub(super) async fn quit(mut self) -> Result<()> {
        debug!("Sending QUIT command");
        let _ = self.command("QUIT").await.ok(); // Ignore errors on quit
        Ok(())
    }
}

/// Parse a successful FTP `SIZE` response within the local file-offset range.
///
/// `aria2_original` parses the value as a signed 64-bit length and rejects
/// values above `a2_off_t::max()`. The Rust download state uses `u64` for
/// progress reporting, but allocation and file offsets still cannot safely
/// represent values above the same signed limit.
pub(super) fn parse_ftp_size_response(response: &str) -> Result<u64> {
    let size = response.trim().parse::<u64>().map_err(|error| {
        Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
            message: format!("Invalid FTP SIZE response {:?}: {}", response, error),
        })
    })?;

    if size > i64::MAX as u64 {
        return Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("FTP SIZE response is too large: {}", size),
            },
        ));
    }

    Ok(size)
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
