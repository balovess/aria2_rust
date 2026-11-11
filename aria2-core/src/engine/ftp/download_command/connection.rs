//! FTP control connection establishment and data-channel TLS.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info};

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use aria2_protocol::ftp::connection::{FtpControlSession, FtpControlSessionError};
use aria2_protocol::ftp::tls::{self as tls, FtpControlStream, FtpDataStream, FtpsClient};

use crate::ftp::connection::{FtpProxyConfig, FtpProxyTunnel, FtpProxyTunnelConfig};
use crate::network::{ConnectionContext, OutboundNetworkPolicy};

// ---------------------------------------------------------------------------
// RawFtpControl
// ---------------------------------------------------------------------------

/// Raw FTP control connection handler
pub(super) struct RawFtpControl {
    pub(super) reader: FtpControlSession<FtpControlStream>,
    pub(super) host: String,
    pub(super) connection: ConnectionContext,
    pub(super) ftps_client: Option<FtpsClient>,
    pub(super) outbound_network_policy: Arc<OutboundNetworkPolicy>,
}

impl RawFtpControl {
    async fn connect_tcp_at(
        host: &str,
        port: u16,
        socket_addr: std::net::SocketAddr,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Result<(tokio::net::TcpStream, ConnectionContext)> {
        let addr = format!("{}:{}", host, port);
        debug!("Connecting to FTP server at {} via {}", addr, socket_addr);

        let stream = outbound_network_policy
            .connect(socket_addr)
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
        ftps_client: Option<FtpsClient>,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Self {
        Self {
            reader: FtpControlSession::new(stream),
            host: host.to_string(),
            connection,
            ftps_client,
            outbound_network_policy,
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

    /// Send a command to the FTP server.
    #[cfg(test)]
    pub(super) async fn send_command(&mut self, cmd: &str) -> Result<()> {
        debug!("FTP CMD: {}", cmd.trim());
        self.reader
            .send_command(cmd)
            .await
            .map_err(map_session_error)?;
        Ok(())
    }

    /// Read response from FTP server with timeout.
    pub(super) async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        let response = self
            .reader
            .read_response(timeout_dur)
            .await
            .map_err(map_session_error)?;
        Ok((response.code, response.message))
    }

    pub(super) fn connection_context(&self) -> &ConnectionContext {
        &self.connection
    }

    pub(super) async fn secure_data_stream(
        &self,
        stream: tokio::net::TcpStream,
    ) -> Result<FtpDataStream> {
        if let Some(client) = &self.ftps_client {
            let tls_stream = client
                .upgrade_data_stream(stream, &self.host)
                .await
                .map_err(|error| {
                    Aria2Error::Network(format!("FTPS data TLS handshake failed: {}", error))
                })?;
            Ok(FtpDataStream::Tls(Box::new(tls_stream)))
        } else {
            Ok(FtpDataStream::Plain(stream))
        }
    }

    /// Send command and read response in one operation.
    #[cfg(test)]
    pub(super) async fn command(&mut self, cmd: &str) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(Duration::from_secs(constants::FTP_COMMAND_TIMEOUT_SECS))
            .await
    }

    pub(super) async fn connect_at(
        host: &str,
        port: u16,
        socket_addr: std::net::SocketAddr,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Result<Self> {
        let (stream, connection) = Self::connect_tcp_at(
            host,
            port,
            socket_addr,
            Arc::clone(&outbound_network_policy),
        )
        .await?;
        let mut ctrl = Self::from_stream(
            FtpControlStream::Plain(stream),
            host,
            connection,
            None,
            outbound_network_policy,
        );
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
        ftps_client: Option<&FtpsClient>,
        ftps_implicit: bool,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
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
        let stream =
            FtpProxyTunnel::establish_with_policy(&tunnel_config, &outbound_network_policy).await?;
        let peer_addr = stream.peer_addr().map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP proxy peer address unavailable: {}", error),
            })
        })?;
        let connection = ConnectionContext::new(host, port, peer_addr);

        if let Some(client) = ftps_client {
            if ftps_implicit {
                let tls_stream =
                    client
                        .perform_tls_handshake(stream, host)
                        .await
                        .map_err(|error| {
                            Aria2Error::Network(format!(
                                "FTPS proxy TLS handshake failed: {}",
                                error
                            ))
                        })?;
                let mut ctrl = Self::from_stream(
                    FtpControlStream::Tls(Box::new(tls_stream)),
                    host,
                    connection,
                    Some(client.clone()),
                    Arc::clone(&outbound_network_policy),
                );
                ctrl.read_welcome().await?;
                tls::negotiate_protected_data_channel(ctrl.reader.reader_mut())
                    .await
                    .map_err(|error| {
                        Aria2Error::Network(format!("FTPS data protection failed: {}", error))
                    })?;
                return Ok(ctrl);
            }

            let mut plain = Self::from_stream(
                FtpControlStream::Plain(stream),
                host,
                connection.clone(),
                None,
                Arc::clone(&outbound_network_policy),
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
            let tls_stream =
                client
                    .upgrade_control_stream(stream, &host)
                    .await
                    .map_err(|error| {
                        Aria2Error::Network(format!("FTPS proxy control upgrade failed: {}", error))
                    })?;
            let mut ctrl = Self::from_stream(
                FtpControlStream::Tls(Box::new(tls_stream)),
                &host,
                connection,
                Some(client.clone()),
                Arc::clone(&outbound_network_policy),
            );
            ctrl.read_welcome().await?;
            return Ok(ctrl);
        }

        let mut ctrl = Self::from_stream(
            FtpControlStream::Plain(stream),
            host,
            connection,
            None,
            outbound_network_policy,
        );
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
        client: &FtpsClient,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Result<Self> {
        let (stream, connection) = Self::connect_tcp_at(
            host,
            port,
            socket_addr,
            Arc::clone(&outbound_network_policy),
        )
        .await?;
        let mut plain = Self::from_stream(
            FtpControlStream::Plain(stream),
            host,
            connection,
            None,
            Arc::clone(&outbound_network_policy),
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
            FtpControlStream::Tls(_) => unreachable!("fresh FTPS control stream is plain"),
        };
        let tls_stream = client
            .upgrade_control_stream(stream, &host)
            .await
            .map_err(|error| {
                Aria2Error::Network(format!("FTPS control upgrade failed: {}", error))
            })?;

        info!("FTPS control connection established with {}:{}", host, port);
        Ok(Self::from_stream(
            FtpControlStream::Tls(Box::new(tls_stream)),
            &host,
            connection,
            Some(client.clone()),
            outbound_network_policy,
        ))
    }

    /// Connect to an implicit FTPS endpoint where TLS starts immediately.
    pub(super) async fn connect_ftps_implicit_at(
        host: &str,
        port: u16,
        socket_addr: std::net::SocketAddr,
        client: &FtpsClient,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Result<Self> {
        let (stream, connection) = Self::connect_tcp_at(
            host,
            port,
            socket_addr,
            Arc::clone(&outbound_network_policy),
        )
        .await?;
        let tls_stream = client
            .perform_tls_handshake(stream, host)
            .await
            .map_err(|error| {
                Aria2Error::Network(format!("FTPS TLS handshake failed: {}", error))
            })?;
        let mut ctrl = Self::from_stream(
            FtpControlStream::Tls(Box::new(tls_stream)),
            host,
            connection,
            Some(client.clone()),
            outbound_network_policy,
        );
        ctrl.read_welcome().await?;

        tls::negotiate_protected_data_channel(ctrl.reader.reader_mut())
            .await
            .map_err(|error| {
                Aria2Error::Network(format!("FTPS data protection failed: {}", error))
            })?;

        info!(
            "Implicit FTPS control connection established with {}:{}",
            host, port
        );
        Ok(ctrl)
    }
}

pub(super) fn map_session_error(error: FtpControlSessionError) -> Aria2Error {
    use aria2_protocol::ftp::connection::control_io::FtpControlReadError;

    match error {
        FtpControlSessionError::ReadTimeout(timeout) => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP response timeout after {timeout:?}"),
            })
        }
        FtpControlSessionError::Read(FtpControlReadError::Timeout) => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "FTP response timeout".into(),
            })
        }
        FtpControlSessionError::Read(FtpControlReadError::Io(error)) => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP read response error: {error}"),
            })
        }
        FtpControlSessionError::Write(error) => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP write command failed: {error}"),
            })
        }
        FtpControlSessionError::InvalidResponse(line) => {
            Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
                message: format!("Invalid FTP response line: {line:?}"),
            })
        }
        FtpControlSessionError::InvalidValue(message) => {
            Aria2Error::Recoverable(RecoverableError::FtpProtocolError { message })
        }
        FtpControlSessionError::Rejected { command, response } => {
            if response.code == 550 && matches!(command, "CWD" | "SIZE" | "RETR") {
                Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
            } else {
                let message = match command {
                    "USER" => format!(
                        "Unexpected USER response: {} {}",
                        response.code, response.message
                    ),
                    "PASS" => format!("Login failed: {} {}", response.code, response.message),
                    "TYPE A" => format!(
                        "TYPE A (ASCII) failed: {} {}",
                        response.code, response.message
                    ),
                    "TYPE I" => format!(
                        "TYPE I (binary) failed: {} {}",
                        response.code, response.message
                    ),
                    "PWD" | "CWD" | "PASV" | "PORT" => {
                        format!("{command} failed: {} {}", response.code, response.message)
                    }
                    "EPRT" => format!(
                        "EPRT failed for IPv6: {} {}",
                        response.code, response.message
                    ),
                    "RETR" => format!(
                        "RETR unexpected response: {} {}",
                        response.code, response.message
                    ),
                    _ => format!("{command} failed: {} {}", response.code, response.message),
                };
                Aria2Error::Recoverable(RecoverableError::FtpProtocolError { message })
            }
        }
        FtpControlSessionError::IncompleteResponse => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "FTP response ended before CRLF-terminated response was complete".into(),
            })
        }
        FtpControlSessionError::ResponseTooLarge(bytes) => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Max FTP recv buffer reached. length={bytes}"),
            })
        }
    }
}
