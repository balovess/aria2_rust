//! FTP command I/O and simple protocol commands
//!
//! Contains low-level command sending/response reading and
//! simple FTP commands (CWD, PWD, ABOR, QUIT).

use crate::error::{Aria2Error, Result};
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use super::types::{FtpClient, FtpResponse};

impl FtpClient {
    /// Change working directory
    ///
    /// # Arguments
    ///
    /// - `path`: Target directory path
    ///
    /// # Errors
    ///
    /// - 550 Directory does not exist or no permission
    pub async fn cwd(&mut self, path: &str) -> Result<()> {
        debug!("Changing working directory: {}", path);
        self.send_command(&format!("CWD {}", path)).await?;
        let resp = self.read_response().await?;

        if resp.is_positive_completion() {
            Ok(())
        } else if resp.code == 550 {
            Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 550 },
            ))
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "CWD command failed: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// Get current working directory
    ///
    /// # Returns
    ///
    /// Returns the current directory path string
    ///
    /// # Errors
    ///
    /// - 500/501/502 Command execution error
    pub async fn pwd(&mut self) -> Result<String> {
        debug!("Querying current working directory");
        self.send_command("PWD").await?;
        let resp = self.read_response().await?;

        if resp.code == 257 {
            // PWD response format: "/path/to/dir" is current directory
            // Typical format: 257 "/path" is current directory
            let msg = resp.message.trim();
            // Extract path within quotes
            if let Some(start) = msg.find('"')
                && let Some(end) = msg.rfind('"')
                && end > start
            {
                let dir = &msg[start + 1..end];
                debug!("Current directory: {}", dir);
                return Ok(dir.to_string());
            }
            Ok(msg.to_string())
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "PWD command failed: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// Query file modification time (MDTM command, RFC 3659)
    ///
    /// Sends `MDTM <path>` and parses the 213 response with format
    /// `YYYYMMDDhhmmss[.sss]`. Returns the modification time as
    /// `SystemTime` in UTC. If the server returns a non-213 response,
    /// returns `Ok(None)` (the command is advisory-only).
    ///
    /// # Arguments
    ///
    /// - `path`: Remote file path (should be percent-decoded)
    ///
    /// # Returns
    ///
    /// `Ok(Some(SystemTime))` on 213 with valid timestamp,
    /// `Ok(None)` if MDTM is unsupported or response is unparseable.
    pub async fn mdtm(&mut self, path: &str) -> Result<Option<std::time::SystemTime>> {
        debug!("Querying file modification time: {}", path);
        self.send_command(&format!("MDTM {}", path)).await?;
        let resp = self.read_response().await?;

        if resp.code != 213 {
            info!(
                "MDTM command returned non-213 response: {} {}",
                resp.code, resp.message
            );
            return Ok(None);
        }

        // Parse 213 YYYYMMDDhhmmss from the response message
        // The message may look like: "213 20240115103000"
        let msg = resp.message.trim();
        // Skip response code if present in message
        let timestamp_str = if let Some(stripped) = msg.strip_prefix("213") {
            stripped.trim()
        } else {
            msg
        };

        // Take first 14 characters (YYYYMMDDhhmmss), drop fractional part
        if timestamp_str.len() < 14 {
            warn!("MDTM response too short to parse: {}", timestamp_str);
            return Ok(None);
        }

        let ts = &timestamp_str[..14];
        match parse_mdtm_timestamp(ts) {
            Some(t) => {
                debug!("MDTM parsed modification time: {:?}", t);
                Ok(Some(t))
            }
            None => {
                warn!("Failed to parse MDTM timestamp: {}", ts);
                Ok(None)
            }
        }
    }

    /// Query file size (SIZE command)
    ///
    /// Sends `SIZE <path>` and parses the 213 response.
    /// Returns `Ok(Some(size))` on success, `Ok(None)` if the server
    /// does not support SIZE or returns a non-213 response.
    ///
    /// # Arguments
    ///
    /// - `path`: Remote file path (should be percent-decoded)
    ///
    /// # Returns
    ///
    /// `Ok(Some(u64))` with file size on 213 response,
    /// `Ok(None)` if SIZE is not supported by server.
    pub async fn size(&mut self, path: &str) -> Result<Option<u64>> {
        debug!("Querying file size: {}", path);
        self.send_command(&format!("SIZE {}", path)).await?;
        let resp = self.read_response().await?;

        if resp.code == 213 {
            let msg = resp.message.trim();
            // Skip response code if present in message
            let size_str = if let Some(stripped) = msg.strip_prefix("213") {
                stripped.trim()
            } else {
                msg
            };
            match size_str.parse::<u64>() {
                Ok(size) => {
                    debug!("File size: {} bytes", size);
                    Ok(Some(size))
                }
                Err(_) => {
                    warn!("Failed to parse SIZE response: {}", size_str);
                    Ok(None)
                }
            }
        } else {
            info!(
                "SIZE command returned non-213 response: {} {}",
                resp.code, resp.message
            );
            Ok(None)
        }
    }

    /// Send REST command to set resume offset
    ///
    /// In the C++ aria2 implementation, REST is sent AFTER the data
    /// connection method (PASV/PORT) is established, not before.
    /// If the server returns a non-350 response and the offset is
    /// non-zero, this is a CANNOT_RESUME error.
    ///
    /// # Arguments
    ///
    /// - `offset`: Byte offset to resume from (0 means no resume)
    ///
    /// # Errors
    ///
    /// Returns `RecoverableError::CannotResume` if the server does not
    /// support REST and the requested offset is non-zero.
    pub async fn rest(&mut self, offset: u64) -> Result<()> {
        if offset == 0 {
            return Ok(());
        }
        debug!("Setting resume offset: {} bytes", offset);
        self.send_command(&format!("REST {}", offset)).await?;
        let resp = self.read_response().await?;

        if resp.code == 350 {
            debug!("REST accepted by server");
            Ok(())
        } else {
            warn!(
                "REST command not accepted by server: {} {}",
                resp.code, resp.message
            );
            // C++ aria2: if offset != 0 and server doesn't support REST, CANNOT_RESUME
            Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::CannotResume,
            ))
        }
    }

    /// Initiate file retrieval (RETR command)
    ///
    /// # Arguments
    ///
    /// - `path`: Remote file path (should be percent-decoded)
    ///
    /// # Errors
    ///
    /// - 550 File not found
    /// - Non-150/125 response
    pub async fn retr(&mut self, path: &str) -> Result<()> {
        debug!("Initiating file retrieval: {}", path);
        self.send_command(&format!("RETR {}", path)).await?;
        let resp = self.read_response().await?;

        if resp.code == 150 || resp.code == 125 {
            Ok(())
        } else if resp.code == 550 {
            Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 550 },
            ))
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "RETR command failed: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// Read the transfer-complete response (226) after data transfer finishes
    ///
    /// Per C++ aria2 FtpFinishDownloadCommand, a non-226 response is not
    /// treated as a fatal error since the data was already received. Returns
    /// `Ok(true)` if 226 was received, `Ok(false)` for other responses.
    pub async fn read_transfer_complete(&mut self) -> Result<bool> {
        let resp = self.read_response().await?;
        if resp.code == 226 {
            debug!("Transfer complete (226): {}", resp.message.trim());
            Ok(true)
        } else {
            warn!(
                "Transfer completion response non-226: {} {}",
                resp.code, resp.message
            );
            Ok(false)
        }
    }

    /// Get the base working directory
    pub fn base_working_dir(&self) -> &str {
        &self.base_working_dir
    }

    /// Set the base working directory (used when reusing pooled connections)
    pub fn set_base_working_dir(&mut self, dir: String) {
        self.base_working_dir = dir;
    }

    /// Abort an in-progress transfer
    ///
    /// Sends the ABOR command to interrupt the current data transfer operation.
    /// Note: ABOR behavior may vary across different servers.
    ///
    /// # Errors
    ///
    /// - Network error (if control connection is already closed)
    pub async fn abort(&mut self) -> Result<()> {
        debug!("Sending ABOR command to abort transfer");

        // The ABOR command is special and requires special handling
        // Some implementations require sending Telnet IP (Interrupt Process) + SYNCH first
        // Simplified here: just send the command directly
        self.send_command("ABOR").await?;

        // Read response (there may be multiple responses: 426 + 226 or 225 + 226, etc.)
        match self.read_response().await {
            Ok(resp) => {
                debug!("ABOR response: {} {}", resp.code, resp.message);

                // There may be a second response
                // Try to read it but do not require it
                let mut buf = String::new();
                match timeout(
                    Duration::from_secs(2),
                    self.control_stream.read_line(&mut buf),
                )
                .await
                {
                    Ok(Ok(n)) if n > 0 => {
                        debug!("ABOR second response: {}", buf.trim());
                    }
                    _ => {}
                }

                Ok(())
            }
            Err(e) => {
                // Connection may be in an inconsistent state after ABOR, this is expected
                warn!(
                    "Connection state abnormal after ABOR command (may be normal): {}",
                    e
                );
                Ok(())
            }
        }
    }

    /// Disconnect from the FTP server
    ///
    /// Sends the QUIT command and gracefully closes the control connection.
    pub async fn quit(mut self) -> Result<()> {
        debug!("Sending QUIT command");

        if let Err(e) = self.send_command("QUIT").await {
            warn!(
                "Failed to send QUIT command (connection may already be closed): {}",
                e
            );
            return Ok(());
        }

        match self.read_response().await {
            Ok(resp) => {
                info!("FTP disconnected: {}", resp.message.trim());
                Ok(())
            }
            Err(e) => {
                warn!("Failed to read QUIT response: {}", e);
                Ok(())
            }
        }
    }

    // ==================== Internal helper methods ====================

    /// Send an FTP command
    ///
    /// Writes the command with \r\n line ending to the control connection.
    pub(super) async fn send_command(&mut self, cmd: &str) -> Result<()> {
        debug!("FTP command: {}", cmd.trim());

        self.control_stream
            .write_all(cmd.as_bytes())
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to send FTP command: {}", e)))?;

        self.control_stream
            .write_all(b"\r\n")
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to send newline: {}", e)))?;

        self.control_stream
            .flush()
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to flush buffer: {}", e)))?;

        Ok(())
    }

    /// Read an FTP response
    ///
    /// Handles multi-line responses, supports standard FTP response format:
    /// - Single line: `NNN text`
    /// - Multi-line: `NNN-text\n...\nNNN text`
    pub(super) async fn read_response(&mut self) -> Result<FtpResponse> {
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();

            let bytes_read = timeout(self.read_timeout, self.control_stream.read_line(&mut line))
                .await
                .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
                .map_err(|e| Aria2Error::Network(format!("Failed to read FTP response: {}", e)))?;

            if bytes_read == 0 {
                break; // Connection closed
            }

            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                continue;
            }

            // Parse 3-digit response code
            let response_code: u16 = trimmed[..3].parse().unwrap_or(0);

            if code.is_none() {
                code = Some(response_code);
            }

            // Determine separator
            let separator = trimmed.as_bytes()[3];

            if separator == b'-' && !is_multiline {
                // Multi-line response start
                is_multiline = true;
                message.push_str(&trimmed[4..]);
                message.push('\n');
            } else if separator == b' ' {
                // Single-line response or multi-line end
                message.push_str(&trimmed[4..]);
                break;
            } else if is_multiline && trimmed.starts_with(&format!("{:3} ", code.unwrap_or(0))) {
                // Multi-line response end marker
                message.push_str(&trimmed[4..]);
                break;
            } else if is_multiline {
                // Multi-line middle line
                message.push_str(&trimmed[4..]);
                message.push('\n');
            }
        }

        let code_val = code.unwrap_or(0);
        debug!("FTP response: {} {}", code_val, message.trim());

        Ok(FtpResponse {
            code: code_val,
            message,
        })
    }
}

/// Parse an MDTM timestamp in `YYYYMMDDhhmmss` format to `SystemTime` (UTC).
///
/// Returns `None` if the string cannot be parsed. Matches the C++ aria2
/// implementation in `FtpConnection::receiveMdtmResponse` which uses
/// `timegm()` to convert to UTC epoch.
fn parse_mdtm_timestamp(s: &str) -> Option<std::time::SystemTime> {
    if s.len() < 14 {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[4..6].parse().ok()?;
    let day: u32 = s[6..8].parse().ok()?;
    let hour: u32 = s[8..10].parse().ok()?;
    let minute: u32 = s[10..12].parse().ok()?;
    let second: u32 = s[12..14].parse().ok()?;

    // Validate ranges
    if !(1990..=2999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    // Convert to Unix epoch using chrono-free approach
    // Days from year 1970 to start of `year`
    let days_since_epoch = days_from_civil(year, month, day)?;
    let secs =
        days_since_epoch * 86400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

/// Calculate days since 1970-01-01 for a given civil date.
/// Uses Howard Hinnant's algorithm for correctness across the full range.
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<u64> {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let doy = (153 * m as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as u64 * 146097 + doe - 719468;
    Some(days)
}
