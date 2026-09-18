//! Shared FTP data-channel negotiation for fresh and pooled sessions.

use std::net::{IpAddr, SocketAddr};

use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};
use tracing::{debug, warn};

use super::capabilities::ServerCapabilities;
use super::control::ControlSession;
use super::parsing::{parse_epsv_response, parse_pasv_response};
use super::types::PasvResult;
use crate::error::{Aria2Error, RecoverableError, Result};

/// Enter passive mode and return the resolved port plus a direct data stream.
///
/// The caller may discard the direct stream when it needs to establish a
/// proxy tunnel to the resolved port instead. Both fresh and pooled control
/// sessions use the same protocol flow here.
pub(super) async fn enter_passive_mode_get_port<C>(
    ctrl: &mut C,
    connect_timeout: Duration,
    caps: &ServerCapabilities,
) -> Result<PasvResult>
where
    C: ControlSession,
{
    if caps.epsv || !caps.mlst_mlsd {
        debug!("Attempting extended passive mode (EPSV)");
        match ctrl.command("EPSV").await {
            Ok((229, message)) => {
                if let Some(port) = parse_epsv_response(&message) {
                    let peer_ip = ctrl.peer_ip()?;
                    debug!(port, "EPSV successful");
                    let data_stream =
                        connect_passive_data(peer_ip, port, connect_timeout, "EPSV").await?;
                    return Ok(PasvResult {
                        port,
                        stream: Some(data_stream),
                    });
                }
                warn!("Failed to parse EPSV response, falling back to PASV");
            }
            Ok((code, _)) if (500..600).contains(&code) => {
                debug!(code, "EPSV rejected, falling back to PASV");
            }
            Ok((code, message)) => {
                debug!(code, %message, "EPSV returned an unexpected response, trying PASV");
            }
            Err(_) => {
                debug!("EPSV not supported (I/O error), trying PASV");
            }
        }
    }

    debug!("Entering passive mode (PASV)");
    let (code, message) = ctrl.command("PASV").await?;
    if code != 227 {
        return Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("PASV failed: {code} {message}"),
            },
        ));
    }

    let (advertised_host, data_port) = parse_pasv_response(&message).ok_or_else(|| {
        Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
            message: "Cannot parse PASV response".into(),
        })
    })?;
    let peer_ip = ctrl.peer_ip()?;
    debug!(
        advertised_host = %advertised_host,
        control_peer = %peer_ip,
        port = data_port,
        "PASV successful; using control peer address"
    );
    let data_stream = connect_passive_data(peer_ip, data_port, connect_timeout, "PASV").await?;
    Ok(PasvResult {
        port: data_port,
        stream: Some(data_stream),
    })
}

/// Enter active mode and accept the server's data connection.
pub(super) async fn enter_active_mode<C>(
    ctrl: &mut C,
    connect_timeout: Duration,
) -> Result<TcpStream>
where
    C: ControlSession,
{
    let local_addr = ctrl.local_addr()?;
    let local_ip = local_addr.ip();
    let listener = TcpListener::bind(super::active_data_bind_addr(local_addr))
        .await
        .map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to bind data port: {error}"),
            })
        })?;
    let data_port = listener
        .local_addr()
        .map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to get listen port: {error}"),
            })
        })?
        .port();

    let protocol_number = match local_ip {
        IpAddr::V4(_) => 1,
        IpAddr::V6(_) => 2,
    };
    let eprt_command = format!("EPRT |{protocol_number}|{local_ip}|{data_port}|");
    debug!("Sending EPRT command");
    let eprt_response = ctrl.command(&eprt_command).await?;

    if eprt_requires_port(eprt_response.0, &eprt_response.1)? {
        warn!("EPRT unavailable, falling back to PORT mode");
        let ipv4_addr = match local_ip {
            IpAddr::V4(address) => address,
            IpAddr::V6(_) => {
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
        let port_command = format!(
            "PORT {},{},{},{},{},{}",
            ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3], p1, p2
        );
        debug!("Sending PORT command");
        let port_response = ctrl.command(&port_command).await?;
        if !(200..300).contains(&port_response.0) {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!(
                        "PORT command failed: {} {}",
                        port_response.0, port_response.1
                    ),
                },
            ));
        }
    }

    debug!(port = data_port, "Waiting for server data connection");
    let (data_stream, _) = timeout(connect_timeout, listener.accept())
        .await
        .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
        .map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to accept data connection: {error}"),
            })
        })?;
    let _ = data_stream.set_nodelay(true);
    debug!("Active mode data connection established");
    Ok(data_stream)
}

fn eprt_requires_port(code: u16, message: &str) -> Result<bool> {
    match code {
        200 => Ok(false),
        500..=502 => Ok(true),
        _ => Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("EPRT command failed: {code} {message}"),
            },
        )),
    }
}

async fn connect_passive_data(
    peer_ip: IpAddr,
    port: u16,
    connect_timeout: Duration,
    mode: &str,
) -> Result<TcpStream> {
    let data_stream = timeout(
        connect_timeout,
        TcpStream::connect(SocketAddr::new(peer_ip, port)),
    )
    .await
    .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
    .map_err(|error| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("{mode} data connection failed: {error}"),
        })
    })?;
    let _ = data_stream.set_nodelay(true);
    Ok(data_stream)
}

#[cfg(test)]
mod tests {
    use super::eprt_requires_port;

    #[test]
    fn eprt_success_keeps_extended_mode() {
        assert!(!eprt_requires_port(200, "OK").unwrap());
    }

    #[test]
    fn eprt_syntax_errors_fall_back_to_port() {
        for code in [500, 501, 502] {
            assert!(eprt_requires_port(code, "unsupported").unwrap());
        }
    }

    #[test]
    fn eprt_unexpected_errors_are_protocol_errors() {
        assert!(eprt_requires_port(530, "not logged in").is_err());
    }
}
