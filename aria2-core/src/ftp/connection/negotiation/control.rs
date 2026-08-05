//! FTP control connection types.
//!
//! Contains `RawFtpControl` (public, used after negotiation), and internal
//! `FreshControl` / `PooledControl` wrappers used during negotiation.

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError, Result};

// =============================================================================
// RawFtpControl - public control connection wrapper
// =============================================================================

/// Raw FTP control connection handler.
///
/// Wraps the control socket for command/response I/O after the
/// negotiation phase, enabling `finish_download()` to read the 226
/// response and optionally pool the connection.
pub struct RawFtpControl {
    pub(super) reader: BufReader<TcpStream>,
    pub(super) host: String,
    pub(super) read_timeout: Duration,
}

impl RawFtpControl {
    /// Build a `RawFtpControl` from an existing buffered control stream.
    ///
    /// This is used internally by `FtpNegotiator::negotiate()` when the
    /// negotiation completes successfully.
    pub fn new(reader: BufReader<TcpStream>, host: String, read_timeout: Duration) -> Self {
        Self {
            reader,
            host,
            read_timeout,
        }
    }

    /// Read the transfer-complete response (226) after data transfer ends.
    ///
    /// Per the C++ `FtpFinishDownloadCommand`, a non-226 response is NOT
    /// treated as a fatal error since the data was already received. The
    /// connection may still be pooled.
    ///
    /// # Returns
    ///
    /// `Ok(true)` if 226 received, `Ok(false)` for any other response.
    pub async fn read_transfer_complete(&mut self) -> Result<bool> {
        match self.read_response(self.read_timeout).await {
            Ok((226, msg)) => {
                debug!("Transfer complete (226): {}", msg.trim());
                Ok(true)
            }
            Ok((code, msg)) => {
                warn!("Transfer completion non-226: {} {}", code, msg);
                Ok(false)
            }
            Err(e) => {
                debug!("Transfer completion timeout/error (may be normal): {}", e);
                Ok(false)
            }
        }
    }

    /// Send QUIT and close the control connection gracefully.
    pub async fn quit(mut self) -> Result<()> {
        debug!("Sending QUIT command");
        if let Err(e) = self.send_command("QUIT").await {
            warn!("Failed to send QUIT (connection may be closed): {}", e);
            return Ok(());
        }
        match self.read_response(self.read_timeout).await {
            Ok(resp) => {
                info!("FTP disconnected: {}", resp.1.trim());
                Ok(())
            }
            Err(e) => {
                warn!("Failed to read QUIT response: {}", e);
                Ok(())
            }
        }
    }

    /// Get a reference to the underlying control stream (for connection pooling).
    pub fn stream(&self) -> &TcpStream {
        self.reader.get_ref()
    }

    /// Consume self and return the inner buffered reader (for connection pooling).
    pub fn into_inner(self) -> BufReader<TcpStream> {
        self.reader
    }

    /// Get the host this control connection is connected to.
    pub fn host(&self) -> &str {
        &self.host
    }

    // ---- Internal I/O helpers ----

    pub(super) async fn send_command(&mut self, cmd: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
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

    pub(super) async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        read_response_impl(&mut self.reader, timeout_dur).await
    }

    /// Send command and read response in one operation.
    #[allow(dead_code)]
    pub(super) async fn command(
        &mut self,
        cmd: &str,
        timeout_dur: Duration,
    ) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(timeout_dur).await
    }
}

// =============================================================================
// FreshControl - control wrapper for freshly established sessions
// =============================================================================

/// Control connection for a freshly established FTP session.
///
/// Wraps the buffered TCP stream with command/response helpers.
pub(super) struct FreshControl {
    pub(super) reader: BufReader<TcpStream>,
    pub(super) command_timeout: Duration,
}

impl FreshControl {
    pub(super) fn peer_ip(&self) -> Result<std::net::IpAddr> {
        self.reader
            .get_ref()
            .peer_addr()
            .map(|address| address.ip())
            .map_err(|error| {
                Aria2Error::Network(format!("FTP control peer unavailable: {}", error))
            })
    }

    pub(super) async fn send_command(&mut self, cmd: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        debug!("FTP CMD: {}", cmd.trim());
        self.reader
            .get_mut()
            .write_all(format!("{}\r\n", cmd).as_bytes())
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP write failed: {}", e),
                })
            })?;
        self.reader.get_mut().flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP flush failed: {}", e),
            })
        })?;
        Ok(())
    }

    pub(super) async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        read_response_impl(&mut self.reader, timeout_dur).await
    }

    pub(super) async fn command(&mut self, cmd: &str) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(self.command_timeout).await
    }
}

// =============================================================================
// PooledControl - control wrapper for pre-authenticated pooled sessions
// =============================================================================

/// Control connection for a pooled (pre-authenticated) FTP session.
///
/// Functionally identical to `FreshControl` but distinct type to prevent
/// mixing fresh and pooled flows.
pub(super) struct PooledControl {
    pub(super) reader: BufReader<TcpStream>,
    pub(super) read_timeout: Duration,
}

impl PooledControl {
    pub(super) fn peer_ip(&self) -> Result<std::net::IpAddr> {
        self.reader
            .get_ref()
            .peer_addr()
            .map(|address| address.ip())
            .map_err(|error| {
                Aria2Error::Network(format!("FTP control peer unavailable: {}", error))
            })
    }

    pub(super) async fn send_command(&mut self, cmd: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        debug!("FTP CMD (pooled): {}", cmd.trim());
        self.reader
            .get_mut()
            .write_all(format!("{}\r\n", cmd).as_bytes())
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP write failed: {}", e),
                })
            })?;
        self.reader.get_mut().flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP flush failed: {}", e),
            })
        })?;
        Ok(())
    }

    pub(super) async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        let (code, msg) = read_response_impl(&mut self.reader, timeout_dur).await?;
        debug!("FTP RESP (pooled): {} {}", code, msg.trim());
        Ok((code, msg))
    }

    pub(super) async fn command(&mut self, cmd: &str) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(self.read_timeout).await
    }
}

// =============================================================================
// Shared response reading implementation
// =============================================================================

/// Maximum receive buffer size for FTP control responses (64KB).
///
/// Matches the C++ `MAX_RECV_BUFFER` constant. Prevents memory exhaustion
/// from malicious servers that send unbounded response data.
const MAX_RECV_BUFFER: usize = 65536;

/// Core multiline FTP response reader shared by all control types.
///
/// This deduplicates the nearly-identical `read_response` implementations
/// that were previously copy-pasted across `RawFtpControl`, `FreshControl`,
/// and `PooledControl`.
///
/// The total accumulated response size is capped at [`MAX_RECV_BUFFER`] to
/// prevent memory exhaustion from malicious or broken servers.
async fn read_response_impl(
    reader: &mut BufReader<TcpStream>,
    timeout_dur: Duration,
) -> Result<(u16, String)> {
    use tokio::io::AsyncBufReadExt;

    let mut line = String::new();
    let mut code: Option<u16> = None;
    let mut message = String::new();
    let mut is_multiline = false;
    let mut total_bytes: usize = 0;

    loop {
        line.clear();
        let bytes_read = timeout(timeout_dur, reader.read_line(&mut line))
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

        // Check buffer size limit – mirrors C++ FtpConnection strbuf_.size()+size guard
        total_bytes += bytes_read;
        if total_bytes > MAX_RECV_BUFFER {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("Max FTP recv buffer reached. length={}", total_bytes),
                },
            ));
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
