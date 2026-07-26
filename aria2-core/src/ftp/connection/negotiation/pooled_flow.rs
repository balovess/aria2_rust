//! Pooled (pre-authenticated) FTP connection negotiation helpers.
//!
//! Contains the `FtpNegotiator` impl methods for reusing a pooled FTP
//! control connection and entering data transfer mode.

use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, warn};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::negotiation::capabilities::ServerCapabilities;
use crate::ftp::connection::negotiation::control::PooledControl;
use crate::ftp::connection::negotiation::parsing::{parse_epsv_response, parse_pasv_response};
use crate::ftp::connection::negotiation::{FtpNegotiator, PasvResult};

impl FtpNegotiator {
    /// Enter passive mode on a pooled connection and return the resolved port + optional stream.
    ///
    /// This method separates port resolution from stream creation, enabling the
    /// C++ `SEQ_RESOLVE_PROXY` flow where the PASV data connection may be
    /// tunneled through an HTTP proxy instead of connected directly.
    pub(super) async fn enter_passive_mode_pooled_get_port(
        ctrl: &mut PooledControl,
        host: &str,
        connect_timeout: Duration,
        caps: &ServerCapabilities,
    ) -> Result<PasvResult> {
        // Try EPSV first if capabilities suggest it or are unknown
        if caps.epsv || !caps.mlst_mlsd {
            debug!("Attempting EPSV (pooled)");
            let epsv_resp = ctrl.command("EPSV").await;

            match epsv_resp {
                Ok(resp) if resp.0 == 229 => {
                    if let Some(port) = parse_epsv_response(&resp.1) {
                        debug!("EPSV successful (pooled), port: {}", port);
                        let data_stream =
                            timeout(connect_timeout, TcpStream::connect((host, port)))
                                .await
                                .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
                                .map_err(|e| {
                                    Aria2Error::Recoverable(
                                        RecoverableError::TemporaryNetworkFailure {
                                            message: format!("EPSV data connection failed: {}", e),
                                        },
                                    )
                                })?;
                        let _ = data_stream.set_nodelay(true);
                        return Ok(PasvResult {
                            port,
                            stream: Some(data_stream),
                        });
                    }
                    warn!("Failed to parse EPSV response, falling back to PASV");
                }
                Ok(resp) if (500..600).contains(&resp.0) => {
                    debug!(
                        "EPSV rejected with {} (pooled), falling back to PASV",
                        resp.0
                    );
                }
                _ => {
                    debug!("EPSV not supported (pooled), trying PASV");
                }
            }
        }

        let pasv_resp = ctrl.command("PASV").await?;
        if pasv_resp.0 != 227 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("PASV failed (pooled): {} {}", pasv_resp.0, pasv_resp.1),
                },
            ));
        }

        match parse_pasv_response(&pasv_resp.1) {
            Some((data_host, data_port)) => {
                debug!("PASV successful (pooled): {}:{}", data_host, data_port);
                let data_stream = timeout(
                    connect_timeout,
                    TcpStream::connect((data_host.as_str(), data_port)),
                )
                .await
                .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("PASV data connection failed: {}", e),
                    })
                })?;
                let _ = data_stream.set_nodelay(true);
                Ok(PasvResult {
                    port: data_port,
                    stream: Some(data_stream),
                })
            }
            None => Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: "Cannot parse PASV response (pooled)".into(),
                },
            )),
        }
    }

    /// Enter passive mode on a pooled connection (convenience wrapper).
    #[allow(dead_code)] // Kept for backward compatibility
    pub(super) async fn enter_passive_mode_pooled(
        ctrl: &mut PooledControl,
        host: &str,
        connect_timeout: Duration,
        caps: &ServerCapabilities,
    ) -> Result<TcpStream> {
        let result =
            Self::enter_passive_mode_pooled_get_port(ctrl, host, connect_timeout, caps).await?;
        Ok(result.stream.unwrap())
    }

    /// Enter active mode on a pooled connection.
    ///
    /// Uses server capabilities to decide EPRT vs PORT order.
    /// Matches C++ `FtpNegotiationCommand` active mode flow.
    pub(super) async fn enter_active_mode_pooled(
        ctrl: &mut PooledControl,
        connect_timeout: Duration,
        _caps: &ServerCapabilities,
    ) -> Result<TcpStream> {
        let local_addr = ctrl.reader.get_ref().local_addr().map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to get local address: {}", e),
            })
        })?;

        let local_ip = local_addr.ip();

        // Bind listener on the appropriate wildcard address for the address family
        let bind_addr = match local_ip {
            std::net::IpAddr::V4(_) => "0.0.0.0:0",
            std::net::IpAddr::V6(_) => "[::]:0",
        };
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to bind data port: {}", e),
                })
            })?;
        let data_port = listener
            .local_addr()
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to get listen port: {}", e),
                })
            })?
            .port();

        // EPRT protocol number: |1| for IPv4, |2| for IPv6 (RFC 2428)
        let proto_num = match local_ip {
            std::net::IpAddr::V4(_) => 1,
            std::net::IpAddr::V6(_) => 2,
        };
        let eprt_cmd = format!("EPRT |{}|{}|{}|", proto_num, local_ip, data_port);
        let eprt_resp = ctrl.command(&eprt_cmd).await?;

        // C++ recvEprt: 200 -> proceed; else -> PORT fallback
        if !(200..300).contains(&eprt_resp.0) {
            let ipv4_addr = match local_ip {
                std::net::IpAddr::V4(v4) => v4,
                std::net::IpAddr::V6(_) => {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::FtpProtocolError {
                            message: "IPv6 does not support PORT command, use passive mode".into(),
                        },
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
            let port_resp = ctrl.command(&port_cmd).await?;
            if !(200..300).contains(&port_resp.0) {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::FtpProtocolError {
                        message: format!(
                            "PORT command failed (pooled): {} {}",
                            port_resp.0, port_resp.1
                        ),
                    },
                ));
            }
        }

        // C++ SEQ_WAIT_CONNECTION
        let (data_stream, _) = timeout(connect_timeout, listener.accept())
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to accept data connection: {}", e),
                })
            })?;
        let _ = data_stream.set_nodelay(true);
        Ok(data_stream)
    }
}
