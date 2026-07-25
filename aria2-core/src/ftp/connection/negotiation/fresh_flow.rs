//! Fresh (non-pooled) FTP connection negotiation helpers.
//!
//! Contains the `FtpNegotiator` impl methods for establishing a new FTP
//! control connection, authenticating, and querying server capabilities
//! (FEAT, OPTS UTF8 ON, SYST).

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::negotiation::capabilities;
use crate::ftp::connection::negotiation::capabilities::ServerCapabilities;
use crate::ftp::connection::negotiation::control::FreshControl;
use crate::ftp::connection::negotiation::parsing::{
    parse_epsv_response, parse_pasv_response,
};
use crate::ftp::connection::negotiation::FtpNegotiator;

impl FtpNegotiator {
    /// Connect to FTP server, read greeting, authenticate, and detect capabilities.
    ///
    /// After successful authentication, sends FEAT to detect server features
    /// and OPTS UTF8 ON if the server advertises UTF8 support (RFC 2640).
    /// This matches the C++ aria2 flow where FEAT/OPTS are sent right after
    /// login on every fresh connection.
    pub(super) async fn connect_and_authenticate(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        connect_timeout: Duration,
        command_timeout: Duration,
    ) -> Result<(FreshControl, ServerCapabilities)> {
        debug!("Connecting to FTP server at {}:{}", host, port);

        let stream = timeout(connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP connect failed to {}:{}: {}", host, port, e),
                })
            })?;

        stream.set_nodelay(true).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {}", e),
            })
        })?;

        let mut ctrl = FreshControl {
            reader: BufReader::new(stream),
            command_timeout,
        };

        // Read welcome message
        let welcome = ctrl.read_response(command_timeout).await?;
        if !(200..300).contains(&welcome.0) && !(100..200).contains(&welcome.0) {
            return Err(Aria2Error::DownloadFailed(format!(
                "FTP server rejected connection: {} {}",
                welcome.0, welcome.1
            )));
        }
        info!("Connected to FTP server {}:{}", host, port);

        // Authenticate using shared helper
        super::parsing::authenticate(&mut ctrl, username, password).await?;

        // Query server capabilities via FEAT command
        let capabilities = capabilities::query_feat(&mut ctrl).await?;

        // If FEAT reports UTF8 support, send OPTS UTF8 ON (RFC 2640)
        if capabilities.utf8 {
            capabilities::send_opts_utf8_on(&mut ctrl).await?;
        }

        Ok((ctrl, capabilities))
    }

    /// Enter passive mode and connect to the data port on a fresh connection.
    ///
    /// Uses server capabilities to decide the order:
    /// - If FEAT reported EPSV support, try EPSV first (with PASV fallback)
    /// - If FEAT did not report EPSV, still try EPSV first but fall back
    ///   more aggressively on 5xx errors (server explicitly refuses EPSV)
    pub(super) async fn enter_passive_mode(
        ctrl: &mut FreshControl,
        host: &str,
        connect_timeout: Duration,
        caps: &ServerCapabilities,
    ) -> Result<TcpStream> {
        // Try EPSV first (IPv6-friendly, RFC 2428)
        if caps.epsv || !caps.mlst_mlsd {
            // If FEAT advertised EPSV, or we have no FEAT info at all,
            // attempt EPSV first.
            debug!("Attempting extended passive mode (EPSV)");
            let epsv_resp = ctrl.command("EPSV").await;

            match epsv_resp {
                Ok(resp) if resp.0 == 229 => {
                    if let Some(port) = parse_epsv_response(&resp.1) {
                        debug!("EPSV successful, using port: {}", port);
                        let data_stream =
                            timeout(connect_timeout, TcpStream::connect((host, port)))
                                .await
                                .map_err(|_| {
                                    Aria2Error::Recoverable(RecoverableError::Timeout)
                                })?
                                .map_err(|e| {
                                    Aria2Error::Recoverable(
                                        RecoverableError::TemporaryNetworkFailure {
                                            message: format!(
                                                "EPSV data connection failed: {}",
                                                e
                                            ),
                                        },
                                    )
                                })?;
                        let _ = data_stream.set_nodelay(true);
                        return Ok(data_stream);
                    }
                    warn!("Failed to parse EPSV response, falling back to PASV");
                }
                Ok(resp) if (500..600).contains(&resp.0) => {
                    // 5xx response: server explicitly refuses EPSV.
                    // This is the critical fallback case for servers that don't
                    // support RFC 2428 at all.
                    debug!(
                        "EPSV rejected with {} (server does not support EPSV), falling back to PASV",
                        resp.0
                    );
                }
                Ok(resp) => {
                    // Other non-229 response (e.g. 4xx temporary)
                    debug!("EPSV unexpected response: {} {}, trying PASV", resp.0, resp.1);
                }
                _ => {
                    debug!("EPSV not supported (I/O error), trying PASV");
                }
            }
        }

        // Fallback to PASV
        debug!("Entering passive mode (PASV)");
        let pasv_resp = ctrl.command("PASV").await?;
        if pasv_resp.0 != 227 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("PASV failed: {} {}", pasv_resp.0, pasv_resp.1),
                },
            ));
        }

        match parse_pasv_response(&pasv_resp.1) {
            Some((data_host, data_port)) => {
                debug!("PASV successful, data channel: {}:{}", data_host, data_port);
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
                Ok(data_stream)
            }
            None => Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "Cannot parse PASV response".into(),
                },
            )),
        }
    }

    /// Enter active mode and accept the server's data connection on a fresh connection.
    ///
    /// Uses server capabilities to decide EPRT vs PORT:
    /// - If FEAT reported EPRT support, try EPRT first
    /// - Fallback to PORT if EPRT fails
    pub(super) async fn enter_active_mode(
        ctrl: &mut FreshControl,
        connect_timeout: Duration,
        _caps: &ServerCapabilities,
    ) -> Result<TcpStream> {
        // Get local address from the control stream
        let local_addr = ctrl
            .reader
            .get_ref()
            .local_addr()
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to get local address: {}", e),
                })
            })?;

        // Bind a listener on port 0 (auto-assign)
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
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

        let local_ip = local_addr.ip();

        // Try EPRT first
        let eprt_cmd = format!("EPRT |1|{}|{}|", local_ip, data_port);
        debug!("Sending EPRT command");
        let eprt_resp = ctrl.command(&eprt_cmd).await?;

        if eprt_resp.0 != 200
            && eprt_resp.0 != 500
            && eprt_resp.0 != 501
            && eprt_resp.0 != 502
        {
            return Err(Aria2Error::DownloadFailed(format!(
                "EPRT command failed: {} {}",
                eprt_resp.0, eprt_resp.1
            )));
        }

        if !(200..300).contains(&eprt_resp.0) {
            // EPRT failed, try PORT
            warn!("EPRT unavailable, falling back to PORT mode");

            let ipv4_addr = match local_ip {
                std::net::IpAddr::V4(v4) => v4,
                std::net::IpAddr::V6(_) => {
                    return Err(Aria2Error::DownloadFailed(
                        "IPv6 does not support active mode PORT command".into(),
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
            debug!("Sending PORT command");
            let port_resp = ctrl.command(&port_cmd).await?;
            if !(200..300).contains(&port_resp.0) {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::ServerError { code: 425 },
                ));
            }
        }

        // Wait for server to connect
        debug!("Waiting for server data connection on port: {}", data_port);
        let (data_stream, _addr) = timeout(connect_timeout, listener.accept())
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to accept data connection: {}", e),
                })
            })?;

        let _ = data_stream.set_nodelay(true);
        debug!("Active mode data connection established");
        Ok(data_stream)
    }
}
