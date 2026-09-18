//! FTP command/response operations for an established control connection.

use std::time::Duration;

use tracing::{debug, info, warn};

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::{
    active_data_bind_addr, cwd_targets, parse_mdtm_timestamp, parse_pwd_response,
    split_decoded_remote_path,
};

pub(super) use super::connection::RawFtpControl;
pub(crate) use crate::ftp::connection::percent_decode as urlencoding_decode;
pub(super) use crate::ftp::connection::{parse_epsv_response, parse_pasv_response};

impl RawFtpControl {
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

    /// Set the configured FTP transfer representation (TYPE I or TYPE A).
    pub(super) async fn set_transfer_type(&mut self, transfer_type: &str) -> Result<()> {
        let (command, label) = if transfer_type.eq_ignore_ascii_case("ascii") {
            ("TYPE A", "ASCII")
        } else {
            ("TYPE I", "binary")
        };
        debug!(command, "Setting FTP transfer mode");
        let resp = self.command(command).await?;
        if resp.0 != 200 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("{} ({}) failed: {} {}", command, label, resp.0, resp.1),
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
