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
                warn!("Connection state abnormal after ABOR command (may be normal): {}", e);
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
            warn!("Failed to send QUIT command (connection may already be closed): {}", e);
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
