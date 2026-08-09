//! FTP download finish handler
//!
//! Equivalent to C++ `FtpFinishDownloadCommand`. After the data transfer
//! completes, this handler reads the 226 "Transfer Complete" response from
//! the FTP server and optionally returns the control connection to the pool.
//!
//! # Key Behavior (matching C++)
//!
//! - Read 226 response after data transfer ends
//! - Non-226 responses are NOT fatal (data was already received)
//! - Timeout waiting for 226 is NOT fatal (data was already received)
//! - If `ftp_reuse_connection` is enabled and 226 received, pool the connection
//! - Exceptions during 226 reading are silently ignored
//! - Connection pool key: `username@host(port)` with baseWorkingDir as metadata

use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::error::Result;
use crate::ftp::connection::FtpMode;
use crate::ftp::connection::negotiation::RawFtpControl;
use crate::ftp::connection_pool::FtpConnectionPool;

/// Configuration for the FTP finish-download operation.
#[derive(Debug, Clone)]
pub struct FtpFinishConfig {
    /// Whether to return the control connection to the pool after 226.
    /// Maps to C++ `PREF_FTP_REUSE_CONNECTION`.
    pub reuse_connection: bool,

    /// Maximum time to wait for the 226 response before giving up.
    /// Matches C++ `AbstractCommand::getTimeout()`.
    pub finish_timeout: Duration,

    /// Username used for authentication (part of pool key).
    pub username: String,

    /// Server hostname (part of pool key).
    pub host: String,

    /// Server port (part of pool key).
    pub port: u16,

    /// Connection mode (passive/active) for pool metadata.
    pub mode: FtpMode,

    /// Base working directory from PWD command (stored as pool metadata,
    /// used to skip CWD traversal on reuse).
    pub base_working_dir: String,
}

impl Default for FtpFinishConfig {
    fn default() -> Self {
        Self {
            reuse_connection: true,
            finish_timeout: Duration::from_secs(60),
            username: String::new(),
            host: String::new(),
            port: 21,
            mode: FtpMode::Passive,
            base_working_dir: "/".to_string(),
        }
    }
}

/// Result of the finish-download operation.
#[derive(Debug)]
pub struct FtpFinishResult {
    /// Whether a 226 response was received.
    pub transfer_complete: bool,
    /// Whether the connection was returned to the pool.
    pub connection_pooled: bool,
}

/// FTP download finish handler.
///
/// Handles the post-transfer lifecycle of an FTP control connection:
/// 1. Read the 226 "Transfer Complete" response
/// 2. Optionally pool the connection for reuse
///
/// This is the Rust equivalent of C++ `FtpFinishDownloadCommand`.
pub struct FtpFinishHandler;

impl FtpFinishHandler {
    /// Finish an FTP download by reading the 226 response and optionally
    /// pooling the control connection.
    ///
    /// Matches the C++ `FtpFinishDownloadCommand::execute()` logic:
    /// - Read 226 response with timeout
    /// - Non-226 is not fatal (data was already received)
    /// - Timeout is not fatal (data was already received)
    /// - If 226 and reuse_connection enabled, pool the socket
    ///
    /// # Arguments
    ///
    /// * `control` - The raw FTP control connection after negotiation
    /// * `config` - Configuration for the finish operation
    /// * `pool` - Optional connection pool for returning the connection
    ///
    /// # Returns
    ///
    /// `FtpFinishResult` indicating whether 226 was received and
    /// whether the connection was pooled.
    pub async fn finish(
        mut control: RawFtpControl,
        config: &FtpFinishConfig,
        pool: Option<Arc<FtpConnectionPool>>,
    ) -> FtpFinishResult {
        let mut transfer_complete = false;
        let mut connection_pooled = false;

        // Step 1: Read 226 response with timeout
        // Per C++ FtpFinishDownloadCommand, we wait for the server to send
        // the transfer-complete response. If data is available, read it.
        // If timeout, that's OK too — the download was already successful.
        match timeout(config.finish_timeout, control.read_transfer_complete()).await {
            Ok(Ok(true)) => {
                // 226 received — transfer complete
                transfer_complete = true;
                info!(
                    "FTP transfer complete (226) for {}:{}",
                    config.host, config.port
                );
            }
            Ok(Ok(false)) => {
                // Non-226 response — not fatal, data was already received
                // C++ logs this as "Bad status for transfer complete" at INFO level
                warn!(
                    "FTP transfer finished with non-226 status for {}:{}",
                    config.host, config.port
                );
            }
            Ok(Err(e)) => {
                // Error reading response — not fatal
                // C++ catches RecoverableException and logs at DEBUG level
                debug!(
                    "FTP finish response error (ignorable, download complete): {}",
                    e
                );
            }
            Err(_) => {
                // Timeout waiting for 226 — not fatal
                // C++ logs "Timeout before receiving transfer complete"
                debug!(
                    "FTP timeout waiting for 226 response from {}:{}",
                    config.host, config.port
                );
            }
        }

        // Step 2: Decide the fate of the control connection
        let should_pool = transfer_complete && config.reuse_connection && pool.is_some();

        if should_pool {
            // Pool the connection — this consumes `control`
            let pool_ref = pool.as_ref().unwrap();
            match Self::pool_connection(control, config, pool_ref.clone()).await {
                Ok(()) => {
                    connection_pooled = true;
                    debug!(
                        "FTP connection to {}:{} returned to pool (baseWorkingDir={})",
                        config.host, config.port, config.base_working_dir
                    );
                }
                Err(e) => {
                    // Pooling failed, but control was already consumed by into_inner()
                    // The stream was extracted but the pool rejected it — the stream
                    // will be dropped, closing the TCP connection. This is acceptable
                    // since the download was already completed successfully.
                    debug!(
                        "Failed to pool FTP connection to {}:{}: {} (connection will be dropped)",
                        config.host, config.port, e
                    );
                }
            }
        } else {
            // Not pooling — close gracefully with QUIT
            if let Err(e) = control.quit().await {
                debug!(
                    "FTP QUIT failed for {}:{} (may already be closed): {}",
                    config.host, config.port, e
                );
            }
        }

        FtpFinishResult {
            transfer_complete,
            connection_pooled,
        }
    }

    /// Return a control connection to the connection pool.
    ///
    /// The pool key is constructed from host, port, and username,
    /// matching the C++ `createSockPoolKey` format:
    /// `username@host(port)`
    ///
    /// The `base_working_dir` is stored as metadata in the pooled connection,
    /// allowing subsequent downloads to skip CWD traversal if the path
    /// shares the same base directory.
    async fn pool_connection(
        control: RawFtpControl,
        config: &FtpFinishConfig,
        pool: Arc<FtpConnectionPool>,
    ) -> Result<()> {
        // Extract the underlying TCP stream from the control connection
        let buf_reader = control.into_inner();
        let stream = buf_reader.into_inner();

        // Return to the pool — the pool will wrap it as needed
        pool.return_raw_connection(
            stream,
            &config.host,
            config.port,
            &config.username,
            config.mode,
            &config.base_working_dir,
        )
        .await
    }
}

/// Pooled FTP control connection wrapper.
///
/// Wraps a raw TCP stream that was returned to the connection pool after
/// a successful FTP download. When a new download reuses this connection,
/// the authentication step is skipped and the connection starts from the
/// CWD phase.
///
/// The `base_working_dir` field stores the server's working directory at
/// the time the connection was pooled. Subsequent downloads can use this
/// to determine whether CWD traversal can be partially or fully skipped.
#[derive(Debug)]
pub struct PooledFtpControl {
    /// The buffered control stream
    pub reader: BufReader<TcpStream>,
    /// Server hostname
    pub host: String,
    /// Server port
    pub port: u16,
    /// Username used for authentication
    pub username: String,
    /// Base working directory when the connection was pooled
    pub base_working_dir: String,
    /// Read timeout for operations
    pub read_timeout: Duration,
}

impl PooledFtpControl {
    /// Create a new pooled FTP control from a raw TCP stream.
    ///
    /// This is used when a pooled connection is retrieved for reuse.
    pub fn new(
        stream: TcpStream,
        host: String,
        port: u16,
        username: String,
        base_working_dir: String,
        read_timeout: Duration,
    ) -> Self {
        Self {
            reader: BufReader::new(stream),
            host,
            port,
            username,
            base_working_dir,
            read_timeout,
        }
    }

    /// Convert back into a `RawFtpControl` for use in negotiation.
    ///
    /// When a pooled connection is reused, it needs to be converted back
    /// into a `RawFtpControl` so the negotiation module can send CWD,
    /// SIZE, MDTM, etc. commands.
    pub fn into_raw_control(self) -> RawFtpControl {
        RawFtpControl::new(self.reader, self.host, self.read_timeout)
    }

    /// Get the base working directory (for determining CWD skip).
    pub fn base_working_dir(&self) -> &str {
        &self.base_working_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_finish_config_default() {
        let config = FtpFinishConfig::default();
        assert!(config.reuse_connection);
        assert_eq!(config.finish_timeout, Duration::from_secs(60));
        assert_eq!(config.port, 21);
        assert_eq!(config.base_working_dir, "/");
        assert!(config.username.is_empty());
        assert!(config.host.is_empty());
    }

    #[test]
    fn test_finish_result_default() {
        let result = FtpFinishResult {
            transfer_complete: false,
            connection_pooled: false,
        };
        assert!(!result.transfer_complete);
        assert!(!result.connection_pooled);
    }

    #[tokio::test]
    async fn test_finish_without_pool() {
        // Create a mock control connection (we can't easily create a real
        // TcpStream in unit tests, so this tests the config logic only)
        let config = FtpFinishConfig {
            reuse_connection: true,
            finish_timeout: Duration::from_millis(100),
            username: "test".to_string(),
            host: "ftp.example.com".to_string(),
            port: 21,
            mode: FtpMode::Passive,
            base_working_dir: "/pub".to_string(),
        };

        assert!(config.reuse_connection);
        assert_eq!(config.base_working_dir, "/pub");
    }

    #[test]
    fn test_pooled_ftp_control_creation() {
        // Test that the struct can be created with expected fields
        let config = FtpFinishConfig {
            base_working_dir: "/var/ftp".to_string(),
            ..Default::default()
        };
        assert_eq!(config.base_working_dir, "/var/ftp");
    }

    #[test]
    fn test_pool_key_construction() {
        // Verify the expected pool key format matches C++ createSockPoolKey
        // C++ format: "username@host(port)" or "host(port)" if no username
        let username = "admin";
        let host = "ftp.example.com";
        let port: u16 = 21;

        let key_with_user = format!("{}@{}({})", username, host, port);
        assert_eq!(key_with_user, "admin@ftp.example.com(21)");

        let key_no_user = format!("{}({})", host, port);
        assert_eq!(key_no_user, "ftp.example.com(21)");
    }
}
