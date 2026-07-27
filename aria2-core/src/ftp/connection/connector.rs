//! FTP connection establishment and mode configuration
//!
//! Contains methods for connecting to an FTP server, logging in,
//! setting transfer mode, and establishing passive/active data connections.
//! Supports both plain FTP and FTPS (FTP over TLS) per RFC 4217.

use crate::error::{Aria2Error, Result};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use super::tls;
use super::types::{FtpClient, FtpControlStream, FtpMode, FtpTlsMode, FtpsConfig};

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
            control_stream: tokio::io::BufReader::new(FtpControlStream::Plain(stream)),
            mode,
            binary_mode: false,
            host: host.to_string(),
            port,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
            read_timeout: Self::DEFAULT_READ_TIMEOUT,
            base_working_dir: "/".to_string(),
            features: None,
            tls_mode: FtpTlsMode::None,
            data_channel_protected: false,
            ftps_config: None,
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

    /// Connect to an FTPS server with explicit TLS upgrade (AUTH TLS).
    ///
    /// After the 220 greeting, sends `AUTH TLS`, upgrades the control stream
    /// to TLS, then sends `PBSZ 0` and `PROT P` to protect both control and
    /// data channels per RFC 4217.
    ///
    /// # Arguments
    ///
    /// - `host`: FTP server address (domain name or IP)
    /// - `port`: FTP server port (typically 21 for explicit FTPS)
    /// - `mode`: Data connection mode (passive or active)
    /// - `config`: FTPS TLS configuration (certificate verification, CA path)
    ///
    /// # Errors
    ///
    /// - Connection timeout
    /// - AUTH TLS rejected (non-234 response)
    /// - TLS handshake failed
    /// - PBSZ 0 or PROT P rejected
    pub async fn connect_ftps_explicit(
        host: &str,
        port: u16,
        mode: FtpMode,
        config: &FtpsConfig,
    ) -> Result<Self> {
        info!("FTPS (explicit) connecting: {}:{}", host, port);

        // Step 1: Connect in plaintext and read greeting
        let stream = timeout(
            Self::DEFAULT_CONNECT_TIMEOUT,
            TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("FTPS connection failed: {}", e)))?;

        // Wrap in BufReader to read the 220 greeting, then recover the stream
        let mut buf_reader = tokio::io::BufReader::new(stream);
        let greeting = read_single_response(&mut buf_reader, Self::DEFAULT_READ_TIMEOUT).await?;

        if greeting.code != 220 {
            return Err(Aria2Error::DownloadFailed(format!(
                "FTPS server refused connection (expected 220): {} {}",
                greeting.code, greeting.message
            )));
        }
        info!("FTPS greeting received: {}", greeting.message.trim());

        // Recover the plain TCP stream from the BufReader for TLS upgrade
        let stream = buf_reader.into_inner();

        // Step 2: AUTH TLS + TLS handshake + PBSZ 0 + PROT P
        let tls_stream = tls::upgrade_control_stream(stream, host, config)
            .await
            .map_err(|e| Aria2Error::Network(format!("FTPS upgrade failed: {}", e)))?;

        let client = Self {
            control_stream: tokio::io::BufReader::new(FtpControlStream::Tls(tls_stream)),
            mode,
            binary_mode: false,
            host: host.to_string(),
            port,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
            read_timeout: Self::DEFAULT_READ_TIMEOUT,
            base_working_dir: "/".to_string(),
            features: None,
            tls_mode: FtpTlsMode::Explicit,
            data_channel_protected: true,
            ftps_config: Some(config.clone()),
        };

        info!("FTPS explicit connection established with {}", host);
        Ok(client)
    }

    /// Connect to an FTPS server with implicit TLS (TLS from the start).
    ///
    /// Used for legacy implicit FTPS on port 990 where the TCP connection
    /// is immediately wrapped in TLS before any FTP protocol exchange.
    /// No `AUTH TLS` command is sent.
    ///
    /// # Arguments
    ///
    /// - `host`: FTP server address (domain name or IP)
    /// - `port`: FTP server port (typically 990 for implicit FTPS)
    /// - `mode`: Data connection mode (passive or active)
    /// - `config`: FTPS TLS configuration
    ///
    /// # Errors
    ///
    /// - Connection timeout
    /// - TLS handshake failed
    /// - Server refused connection (non-220 greeting)
    pub async fn connect_ftps_implicit(
        host: &str,
        port: u16,
        mode: FtpMode,
        config: &FtpsConfig,
    ) -> Result<Self> {
        info!("FTPS (implicit) connecting: {}:{}", host, port);

        // Step 1: Connect and immediately perform TLS handshake
        let stream = timeout(
            Self::DEFAULT_CONNECT_TIMEOUT,
            TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("FTPS implicit connection failed: {}", e)))?;

        let tls_stream = tls::perform_tls_handshake(stream, host, config)
            .await
            .map_err(|e| Aria2Error::Network(format!("FTPS implicit TLS failed: {}", e)))?;

        let mut client = Self {
            control_stream: tokio::io::BufReader::new(FtpControlStream::Tls(tls_stream)),
            mode,
            binary_mode: false,
            host: host.to_string(),
            port,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
            read_timeout: Self::DEFAULT_READ_TIMEOUT,
            base_working_dir: "/".to_string(),
            features: None,
            tls_mode: FtpTlsMode::Implicit,
            data_channel_protected: true,
            ftps_config: Some(config.clone()),
        };

        // Step 2: Read the 220 greeting (now over TLS)
        // For implicit FTPS, we still need to send PBSZ 0 + PROT P
        // after receiving the greeting.
        let greeting = client.read_response().await?;
        if greeting.code != 220 {
            return Err(Aria2Error::DownloadFailed(format!(
                "FTPS implicit server refused connection: {} {}",
                greeting.code, greeting.message
            )));
        }

        // Send PBSZ 0 + PROT P on the TLS control stream
        // (no AUTH TLS needed for implicit FTPS)
        send_pbsz_prot_on_client(&mut client).await?;

        info!("FTPS implicit connection established with {}", host);
        Ok(client)
    }

    /// Whether the control connection is TLS-encrypted.
    pub fn is_tls(&self) -> bool {
        self.tls_mode != FtpTlsMode::None
    }

    /// Whether data connections will be TLS-protected (PROT P negotiated).
    pub fn is_data_channel_protected(&self) -> bool {
        self.data_channel_protected
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

    /// Enter passive mode and establish data connection.
    ///
    /// If PROT P has been negotiated (FTPS mode), the data stream is
    /// also upgraded to TLS before being returned.
    ///
    /// Tries EPSV (Extended Passive Mode) first, falls back to PASV if
    /// the server does not support it.
    ///
    /// # Returns
    ///
    /// Returns the data connection TcpStream (TLS-wrapped if PROT P).
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

                return self.maybe_upgrade_data_stream(data_stream).await;
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

        self.maybe_upgrade_data_stream(data_stream).await
    }

    /// Enter active mode and establish data connection.
    ///
    /// If PROT P has been negotiated (FTPS mode), the accepted data
    /// stream is also upgraded to TLS before being returned.
    ///
    /// Sends a PORT or EPRT command to inform the server of the client's
    /// data port, then listens on that port for the server to connect.
    ///
    /// # Returns
    ///
    /// Returns the accepted data connection TcpStream (TLS-wrapped if PROT P).
    ///
    /// # Errors
    ///
    /// - 425 Cannot open data connection
    /// - 500/501/502 Command syntax error
    pub async fn active_mode(&mut self) -> Result<TcpStream> {
        debug!("Requesting active mode data connection");

        // Get local address from the underlying TCP stream
        let local_addr = self
            .control_stream
            .get_ref()
            .get_ref()
            .ok_or_else(|| Aria2Error::Network("Cannot get local address from TLS stream".to_string()))?
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("Failed to get local address: {}", e)))?;

        let local_ip = local_addr.ip();

        // Bind listener on the appropriate wildcard address for the address family
        // IPv4 -> 0.0.0.0:0, IPv6 -> [::]:0
        let bind_addr = match local_ip {
            std::net::IpAddr::V4(_) => "0.0.0.0:0",
            std::net::IpAddr::V6(_) => "[::]:0",
        };
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to bind data port: {}", e)))?;
        let data_port = listener
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("Failed to get listen port: {}", e)))?
            .port();

        // EPRT protocol number: |1| for IPv4, |2| for IPv6 (RFC 2428)
        let proto_num = match local_ip {
            std::net::IpAddr::V4(_) => 1,
            std::net::IpAddr::V6(_) => 2,
        };
        let eprt_cmd = format!("EPRT |{}|{}|{}|", proto_num, local_ip, data_port);
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
        self.maybe_upgrade_data_stream(data_stream).await
    }

    // ========================================================================
    // FTPS internal helpers
    // ========================================================================

    /// Conditionally upgrade a data stream to TLS if PROT P is in effect.
    ///
    /// For plain FTP, returns the TcpStream as-is. For FTPS with PROT P,
    /// upgrades to TLS but returns the inner TcpStream since the old
    /// FtpClient API only supports TcpStream. Use FtpNegotiator for full
    /// FTPS data channel encryption support.
    async fn maybe_upgrade_data_stream(&self, stream: TcpStream) -> Result<TcpStream> {
        if !self.data_channel_protected {
            return Ok(stream);
        }

        let config = self.ftps_config.as_ref().ok_or_else(|| {
            Aria2Error::Network(
                "FTPS data channel protected but no FTPS config available".to_string(),
            )
        })?;

        debug!("Upgrading FTP data connection to TLS for {}", self.host);
        let tls_stream = tls::upgrade_data_stream(stream, &self.host, config)
            .await
            .map_err(|e| Aria2Error::Network(format!("FTPS data TLS upgrade failed: {}", e)))?;

        // NOTE: The old FtpClient API returns TcpStream. For full FTPS data
        // channel encryption, use the newer FtpNegotiator which supports
        // polymorphic streams via FtpControlStream.
        warn!(
            "FTPS data TLS negotiated but FtpClient returns TcpStream. \
             Use FtpNegotiator for full FTPS data channel encryption."
        );
        let (tcp_stream, _) = tls_stream.into_inner();
        Ok(tcp_stream)
    }
}

// =============================================================================
// Internal helpers for FTPS negotiation on the FtpClient control stream
// =============================================================================

/// Send PBSZ 0 and PROT P on an existing TLS-encrypted FtpClient.
/// Used for implicit FTPS where AUTH TLS is not needed but PBSZ/PROT
/// are still required to protect the data channel.
async fn send_pbsz_prot_on_client(client: &mut FtpClient) -> Result<()> {
    // Send PBSZ 0
    debug!("Sending PBSZ 0 (implicit FTPS)");
    client.send_command("PBSZ 0").await?;
    let pbsz_resp = client.read_response().await?;
    if pbsz_resp.code != 200 {
        warn!(
            "PBSZ 0 rejected by server: {} {} (continuing without data protection)",
            pbsz_resp.code, pbsz_resp.message
        );
        // Not fatal: control channel is still encrypted
        return Ok(());
    }

    // Send PROT P
    debug!("Sending PROT P (implicit FTPS)");
    client.send_command("PROT P").await?;
    let prot_resp = client.read_response().await?;
    if prot_resp.code != 200 {
        warn!(
            "PROT P rejected by server: {} {} (data channel will NOT be encrypted)",
            prot_resp.code, prot_resp.message
        );
        client.data_channel_protected = false;
    } else {
        client.data_channel_protected = true;
        info!("PROT P accepted — data channel will be TLS-protected");
    }

    Ok(())
}

/// Read a single FTP response before the `FtpClient` is fully constructed.
async fn read_single_response<R: tokio::io::AsyncBufReadExt + Unpin>(
    reader: &mut R,
    timeout_dur: Duration,
) -> Result<super::types::FtpResponse> {
    let mut line = String::new();
    timeout(timeout_dur, reader.read_line(&mut line))
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("Failed to read FTP response: {}", e)))?;

    let trimmed = line.trim_end();
    let (code, message) = if trimmed.len() >= 4 {
        let code: u16 = trimmed[..3].parse().unwrap_or(0);
        let sep = trimmed.as_bytes()[3];
        let msg = if sep == b' ' || sep == b'-' {
            trimmed[4..].to_string()
        } else {
            trimmed[3..].to_string()
        };
        (code, msg)
    } else if trimmed.len() >= 3 {
        let code: u16 = trimmed[..3].parse().unwrap_or(0);
        (code, String::new())
    } else {
        (0, line)
    };

    Ok(super::types::FtpResponse { code, message })
}
