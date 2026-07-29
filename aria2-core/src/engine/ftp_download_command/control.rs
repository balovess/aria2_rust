//! Raw FTP control channel handler and PASV/EPSV response parsers.
//!
//! Contains the `RawFtpControl` struct for managing FTP command/response
//! interactions, plus helper functions for parsing passive mode responses
//! and URL-encoded strings.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

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
    reader: BufReader<tokio::net::TcpStream>,
    host: String,
}

impl RawFtpControl {
    /// Establish connection to FTP server and read welcome message
    pub(super) async fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = format!("{}:{}", host, port);
        let socket_addr: std::net::SocketAddr = addr.parse().map_err(|_| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("cannot parse address: {}", addr),
            })
        })?;

        debug!("Connecting to FTP server at {}:{}", host, port);

        let stream = tokio::net::TcpStream::connect(socket_addr)
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP connect failed to {}:{}: {}", host, port, e),
                })
            })?;

        // Set TCP keepalive and no-delay options
        stream.set_nodelay(true).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {}", e),
            })
        })?;

        let mut ctrl = Self {
            reader: BufReader::new(stream),
            host: host.to_string(),
        };
        let welcome = ctrl
            .read_response(Duration::from_secs(constants::FTP_WELCOME_TIMEOUT_SECS))
            .await?;

        if !(200..300).contains(&welcome.0) && !(100..200).contains(&welcome.0) {
            return Err(Aria2Error::Fatal(FatalError::Config(format!(
                "FTP server rejected connection: {} {}",
                welcome.0, welcome.1
            ))));
        }

        info!("Connected to FTP server {}:{}", host, port);
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
    pub(super) async fn set_resume_offset(&mut self, offset: u64) -> Result<()> {
        if offset == 0 {
            return Ok(());
        }
        debug!("Setting resume offset: {} bytes", offset);
        let resp = self.command(&format!("REST {}", offset)).await?;
        if resp.0 != 350 {
            warn!("REST command not accepted by server: {} {}", resp.0, resp.1);
            // Some servers don't support REST, continue without resume
            return Ok(());
        }
        Ok(())
    }

    /// Get file size (SIZE command)
    pub(super) async fn get_file_size(&mut self, remote_path: &str) -> Result<Option<u64>> {
        debug!("Querying file size: {}", remote_path);
        let resp = self.command(&format!("SIZE {}", remote_path)).await?;
        if resp.0 == 213 {
            let size: u64 = resp.1.trim().parse().unwrap_or(0);
            debug!("File size: {} bytes", size);
            return Ok(Some(size));
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
