//! SFTP Download Command
//!
//! Implements the complete execution logic for SFTP downloads within the aria2
//! download engine. This command integrates the SFTP protocol layer with the
//! engine's RequestGroup, DiskWriter, and rate limiting infrastructure.
//!
//! ## Execution Flow
//!
//! ```text
//! execute()
//!   -> 1. Parse/validate URI and options
//!   -> 2. Establish SSH connection (with auth retry)
//!   -> 3. Initialize SFTP subsystem (version negotiation)
//!   -> 4. STAT remote file (get size, verify existence)
//!   -> 5. Prepare local output (create dir, open disk writer)
//!   -> 6. Chunked read loop:
//!        READ(remote_handle, offset, buf) -> WRITE(disk_writer, chunk) -> UPDATE_PROGRESS
//!   -> 7. Cleanup (close handles, disconnect)
//!   -> 8. Mark RequestGroup as complete
//! ```

use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

// Re-export protocol layer types for use in this module
use aria2_protocol::sftp::connection::{HostKeyCheckingMode, SshConnection, SshError, SshOptions};
use aria2_protocol::sftp::file_ops::{FileOpError, OpenFlags};
use aria2_protocol::sftp::session::SftpSession;

/// Default TCP connection timeout for SFTP downloads
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;
/// Default read operation timeout
const DEFAULT_READ_TIMEOUT_SECS: u64 = 30;
/// Default total command timeout (5 minutes)
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 300;
/// Size of each chunk when writing to disk via disk writer
const DISK_WRITE_CHUNK_SIZE: usize = 64 * 1024; // 64KB
/// Interval in milliseconds between speed calculations
const SPEED_UPDATE_INTERVAL_MS: u128 = 500;

// =============================================================================
// SFTP Download Command
// =============================================================================

/// Command that executes an SFTP file download from a remote server to local disk.
///
/// This is the primary integration point between the aria2 download engine and
/// the SFTP protocol layer. It manages the full lifecycle of an SFTP download
/// including connection management, authentication, data transfer, progress tracking,
/// and cleanup.
pub struct SftpDownloadCommand {
    /// The request group that owns this download (tracks state, progress, etc.)
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    /// Local filesystem path where the downloaded file will be written
    output_path: std::path::PathBuf,
    /// Whether the command has started executing (prevents double-start)
    started: bool,
    /// Total bytes completed so far (for progress tracking)
    completed_bytes: u64,
    /// Remote server hostname or IP
    host: String,
    /// Remote server port (typically 22)
    port: u16,
    /// Username for SSH authentication
    username: String,
    /// Password for authentication (optional if using key-based auth)
    password: Option<String>,
    /// Path to the file on the remote SFTP server
    remote_path: String,
}

impl SftpDownloadCommand {
    /// Create a new SFTP download command.
    ///
    /// # Arguments
    /// * `gid` - Unique group identifier for this download
    /// * `uri` - The sftp:// URI to download from
    /// * `options` - Download configuration options
    /// * `output_dir` - Optional override for output directory
    /// * `output_name` - Optional override for output filename
    ///
    /// # URI Format
    /// ```text
    /// sftp://[user[:password]@]host[:port]/path/to/file
    ///
    /// Examples:
    ///   sftp://user@example.com/path/to/file.txt
    ///   sftp://admin:secret@192.168.1.100:2222/data/archive.tar.gz
    ///   sftp://root@server.example.com:22/etc/config.conf
    /// ```
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        // Step 1: Parse the SFTP URI into components
        let (host, port, username, password, remote_path) = Self::parse_uri(uri)?;

        // Step 2: Determine output directory
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        // Step 3: Determine output filename
        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(&remote_path))
            .unwrap_or_else(|| "download".to_string());

        // Step 4: Build full output path
        let path = std::path::PathBuf::from(&dir).join(&filename);

        // Step 5: Create the request group
        let group = RequestGroup::new(gid, vec![uri.to_string()], options.clone());

        info!(
            "[SFTP-CMD] Created download command: {} -> {} ({}@{}:{}/{})",
            uri,
            path.display(),
            username,
            host,
            port,
            remote_path
        );

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            output_path: path,
            started: false,
            completed_bytes: 0,
            host,
            port,
            username,
            password,
            remote_path,
        })
    }

    /// Parse an sftp:// URI into its component parts.
    ///
    /// Supports the following formats:
    /// - `sftp://user@host/path`
    /// - `sftp://user:password@host/path`
    /// - `sftp://user@host:port/path`
    /// - `sftp://user:password@host:port/path`
    fn parse_uri(uri: &str) -> Result<(String, u16, String, Option<String>, String)> {
        if !uri.starts_with("sftp://") {
            return Err(Aria2Error::Fatal(FatalError::UnsupportedProtocol {
                protocol: "sftp".into(),
            }));
        }

        let without_scheme = uri.trim_start_matches("sftp://");

        // Split authority from path
        let (auth_host_port, path) = match without_scheme.find('/') {
            Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
            None => (without_scheme, "/"),
        };

        // Split user info from host:port
        let (username, rest) = match auth_host_port.find('@') {
            Some(idx) => (&auth_host_port[..idx], &auth_host_port[idx + 1..]),
            None => {
                // No username in URI; try environment variable
                let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
                return Ok((user.to_string(), 22, user, None, "/".to_string()));
            }
        };

        // Extract optional password from username
        let password = username.split(':').nth(1).map(|p| p.to_string());
        let clean_user = username.split(':').next().unwrap_or(username).to_string();

        // Split host from port
        let (host, port) = match rest.rfind(':') {
            Some(idx) => (
                rest[..idx].to_string(),
                rest[idx + 1..].parse::<u16>().unwrap_or(22),
            ),
            None => (rest.to_string(), 22),
        };

        Ok((host, port, clean_user, password, sftp_path_decode(path)))
    }

    /// Extract the filename component from a remote path.
    fn extract_filename(remote_path: &str) -> Option<String> {
        remote_path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "/")
            .map(|s| s.to_string())
    }

    /// Get a read-only reference to the request group.
    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }

    /// Build SshOptions from the command's stored credentials.
    fn build_ssh_options(&self) -> SshOptions {
        let mut opts = SshOptions::new(&self.host, &self.username)
            .with_port(self.port)
            .with_timeouts(
                Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
                Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS),
            )
            .with_host_key_mode(HostKeyCheckingMode::AcceptNew);

        if let Some(ref pwd) = self.password {
            opts = opts.with_password(pwd);
        }

        opts
    }

    /// Map an SshError to the appropriate Aria2Error for engine-level handling.
    fn map_ssh_error(err: &SshError, host: &str, port: u16, _path: &str) -> Aria2Error {
        match err {
            SshError::AuthFailed { .. } => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("{}:{}", host, port),
            }),
            SshError::ConnectTimeout { .. } | SshError::ConnectFailed { .. } => {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: err.to_string(),
                })
            }
            SshError::Handshake { .. } | SshError::ConnectionLost { .. } => {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: err.to_string(),
                })
            }
            SshError::NoCredentials { .. } => Aria2Error::Fatal(FatalError::Config(format!(
                "No SSH credentials provided for {}:{}",
                host, port
            ))),
            _ => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("SFTP error [{}:{}]: {}", host, port, err),
            }),
        }
    }

    /// Map a FileOpError to the appropriate Aria2Error for engine-level handling.
    fn map_file_op_error(err: &FileOpError, host: &str, path: &str) -> Aria2Error {
        match err {
            FileOpError::NotFound { .. } => Aria2Error::Fatal(FatalError::FileNotFound {
                path: path.to_string(),
            }),
            FileOpError::PermissionDenied { .. } => {
                Aria2Error::Fatal(FatalError::PermissionDenied {
                    path: format!("{}:{}", host, path),
                })
            }
            FileOpError::Network { .. } => {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: err.to_string(),
                })
            }
            _ => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("SFTP file op error: {}", err),
            }),
        }
    }
}

/// Decode URL-encoded characters in an SFTP path (%XX sequences).
/// Decode a percent-encoded SFTP path, handling UTF-8 multi-byte sequences correctly.
/// For example, "%E6%96%87%E4%BB%B6" should decode to the Chinese characters for "file".
fn sftp_path_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                bytes.push(byte);
            } else {
                // Invalid percent-encoding, push literal characters
                bytes.extend_from_slice(c.to_string().as_bytes());
                bytes.extend_from_slice(hex.as_bytes());
            }
        } else {
            bytes.push(c as u8);
        }
    }
    // Decode the full byte sequence as UTF-8, with lossy fallback for invalid sequences
    String::from_utf8_lossy(&bytes).into_owned()
}

#[async_trait]
impl Command for SftpDownloadCommand {
    /// Execute the SFTP download.
    ///
    /// This is the main entry point called by the download engine. It orchestrates
    /// the entire download lifecycle from connection establishment through data
    /// transfer to cleanup.
    async fn execute(&mut self) -> Result<()> {
        // -----------------------------------------------------------------
        // Phase 0: Initialization
        // -----------------------------------------------------------------
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        debug!(
            "[SFTP-CMD] Starting download: {}@{}:{} -> {}",
            self.username,
            self.host,
            self.port,
            self.output_path.display()
        );

        // Ensure output directory exists
        if let Some(parent) = self.output_path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "Failed to create output directory: {}",
                    e
                )))
            })?;
        }

        // -----------------------------------------------------------------
        // Phase 1: SSH Connection
        // -----------------------------------------------------------------
        let ssh_options = self.build_ssh_options();
        let conn_result = SshConnection::connect(ssh_options.clone()).await;

        let mut conn = match conn_result {
            Ok(c) => c,
            Err(e) => {
                return Err(Self::map_ssh_error(
                    &e,
                    &self.host,
                    self.port,
                    &self.remote_path,
                ));
            }
        };

        info!(
            "[SFTP-CMD] SSH connected: {}@{}:{}",
            self.username, self.host, self.port
        );

        // -----------------------------------------------------------------
        // Phase 2: SFTP Session Initialization
        // -----------------------------------------------------------------
        let session_result = SftpSession::open(&mut conn).await;

        let session: aria2_protocol::sftp::session::SftpSession = match session_result {
            Ok(s) => s,
            Err(e) => {
                // Attempt graceful disconnect before returning error
                let _ = conn.disconnect().await;
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("SFTP session init failed: {}", e),
                    },
                ));
            }
        };

        debug!(
            "[SFTP-CMD] SFTP session established (v{})",
            session.server_version()
        );

        // -----------------------------------------------------------------
        // Phase 3: Stat Remote File
        // -----------------------------------------------------------------
        use aria2_protocol::sftp::file_ops::SftpFileOps;
        let ops = SftpFileOps::new(&session);

        let file_attrs = match ops.stat(&self.remote_path).await {
            Ok(attrs) => attrs,
            Err(e) => {
                let _ = conn.disconnect().await;
                return Err(Self::map_file_op_error(&e, &self.host, &self.remote_path));
            }
        };

        if !file_attrs.is_regular_file {
            let _ = conn.disconnect().await;
            return Err(Aria2Error::Fatal(FatalError::FileNotFound {
                path: format!("{} (not a regular file)", self.remote_path),
            }));
        }

        let total_length = file_attrs.size;
        info!(
            "[SFTP-CMD] Remote file size: {} bytes ({:.2} MB)",
            total_length,
            total_length as f64 / (1024.0 * 1024.0)
        );

        // Update RequestGroup with total length
        {
            let mut g = self.group.write().await;
            g.set_total_length(total_length).await;
        }

        // -----------------------------------------------------------------
        // Phase 4: Open Remote File for Reading
        // -----------------------------------------------------------------
        let remote_file: aria2_protocol::sftp::file_ops::SftpFileHandle =
            match ops.open(&self.remote_path, OpenFlags::readonly(), 0).await {
                Ok(f) => f,
                Err(e) => {
                    let _ = conn.disconnect().await;
                    return Err(Self::map_file_op_error(&e, &self.host, &self.remote_path));
                }
            };

        // -----------------------------------------------------------------
        // Phase 5: Prepare Local Disk Writer
        // -----------------------------------------------------------------
        let raw_writer = DefaultDiskWriter::new(&self.output_path);

        // Apply rate limiting if configured
        let rate_limit = {
            let g = self.group.read().await;
            g.options().max_download_limit
        };
        let mut writer: Box<dyn DiskWriter> = match rate_limit {
            Some(rate) if rate > 0 => Box::new(ThrottledWriter::new(
                raw_writer,
                RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)),
            )),
            _ => Box::new(raw_writer),
        };

        // -----------------------------------------------------------------
        // Phase 6: Main Download Loop (Chunked Read + Write)
        // -----------------------------------------------------------------
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed: u64 = 0;
        let _buf = vec![0u8; DISK_WRITE_CHUNK_SIZE];

        info!("[SFTP-CMD] Starting transfer loop: {} bytes", total_length);

        loop {
            let remaining = total_length.saturating_sub(self.completed_bytes);
            if remaining == 0 {
                break; // Download complete
            }

            // Calculate how much to read this iteration
            let to_read = (DISK_WRITE_CHUNK_SIZE as u64).min(remaining) as usize;

            // Read chunk from remote file at current offset
            let data = match remote_file
                .read_at(self.completed_bytes, to_read as u32)
                .await
            {
                Ok(data) if data.is_empty() => {
                    debug!(
                        "[SFTP-CMD] EOF at offset {} (expected {})",
                        self.completed_bytes, total_length
                    );
                    break;
                }
                Ok(data) => data,
                Err(e) => {
                    error!(
                        "[SFTP-CMD] Read error at offset {}: {}",
                        self.completed_bytes, e
                    );
                    let _ = remote_file.close().await;
                    let _ = conn.disconnect().await;
                    return Err(Self::map_file_op_error(&e, &self.host, &self.remote_path));
                }
            };
            let n = data.len();

            // Write chunk to local disk via disk writer
            if let Err(e) = writer.write(&data).await {
                error!("[SFTP-CMD] Disk write error: {}", e);
                let _ = remote_file.close().await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::Fatal(FatalError::Config(format!(
                    "Disk write failed: {}",
                    e
                ))));
            }

            self.completed_bytes += n as u64;

            // Update progress in RequestGroup
            {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;

                // Periodic speed calculation (every ~500ms)
                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= SPEED_UPDATE_INTERVAL_MS {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        // -----------------------------------------------------------------
        // Phase 7: Finalize and Cleanup
        // -----------------------------------------------------------------

        // Close remote file handle
        if let Err(e) = remote_file.close().await {
            warn!("[SFTP-CMD] Warning closing remote file: {}", e);
        }

        // Finalize disk writer (flush, sync, etc.)
        if let Err(e) = writer.finalize().await {
            warn!("[SFTP-CMD] Warning finalizing disk writer: {}", e);
        }

        // Disconnect SSH session
        if let Err(e) = conn.disconnect().await {
            warn!("[SFTP-CMD] Warning during SSH disconnect: {}", e);
        }

        // Calculate final statistics
        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
            }
        };

        // Mark RequestGroup as complete
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            g.complete().await?;
        }

        info!(
            "[SFTP-CMD] Download complete: {} ({} bytes, {:.1} KB/s)",
            self.output_path.display(),
            self.completed_bytes,
            final_speed as f64 / 1024.0
        );

        Ok(())
    }

    /// Return the current status of this command.
    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    /// Return the timeout for this command.
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(DEFAULT_COMMAND_TIMEOUT_SECS))
    }
}

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sftp_path_decoding() {
        assert_eq!(sftp_path_decode("/normal/path"), "/normal/path");
        assert_eq!(
            sftp_path_decode("/path%20with%20spaces"),
            "/path with spaces"
        );
        // UTF-8 encoded Chinese path: "%E6%96%87%E4%BB%B6" decodes to "文件"
        assert_eq!(sftp_path_decode("/%E6%96%87%E4%BB%B6"), "/\u{6587}\u{4EF6}");
        assert_eq!(sftp_path_decode("%2Froot%2Ftest"), "/root/test");
    }

    #[test]
    fn test_build_ssh_options_with_password() {
        let cmd = create_test_cmd();
        let opts = cmd.build_ssh_options();
        assert_eq!(opts.host, "example.com");
        assert_eq!(opts.port, 2222);
        assert_eq!(opts.username, "testuser");
        assert_eq!(opts.password.as_deref(), Some("secretpass"));
        assert_eq!(
            opts.connect_timeout,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS)
        );
    }

    #[test]
    fn test_build_ssh_options_without_password() {
        let cmd = SftpDownloadCommand {
            group: Arc::new(tokio::sync::RwLock::new(RequestGroup::new(
                GroupId::new(99),
                vec!["sftp://user@host/file".to_string()],
                DownloadOptions::default(),
            ))),
            output_path: std::path::PathBuf::from("/tmp/out"),
            started: false,
            completed_bytes: 0,
            host: "host".to_string(),
            port: 22,
            username: "user".to_string(),
            password: None,
            remote_path: "/file".to_string(),
        };
        let opts = cmd.build_ssh_options();
        assert!(opts.password.is_none());
    }

    #[test]
    fn test_map_ssh_auth_error_to_fatal() {
        let err = SshError::AuthFailed {
            method: "password".into(),
            message: "bad pass".into(),
        };
        let mapped = SftpDownloadCommand::map_ssh_error(&err, "h", 22, "/f");
        assert!(matches!(
            mapped,
            Aria2Error::Fatal(FatalError::PermissionDenied { .. })
        ));
    }

    #[test]
    fn test_map_ssh_connect_timeout_to_recoverable() {
        let err = SshError::ConnectTimeout {
            host: "h".into(),
            port: 22,
            timeout_secs: 15,
        };
        let mapped = SftpDownloadCommand::map_ssh_error(&err, "h", 22, "/f");
        assert!(matches!(
            mapped,
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
        ));
    }

    #[test]
    fn test_map_file_not_found_to_fatal() {
        let err = FileOpError::NotFound {
            path: "/missing".to_string(),
        };
        let mapped = SftpDownloadCommand::map_file_op_error(&err, "host", "/missing");
        assert!(matches!(
            mapped,
            Aria2Error::Fatal(FatalError::FileNotFound { .. })
        ));
    }

    #[test]
    fn test_map_permission_denied_to_fatal() {
        let err = FileOpError::PermissionDenied {
            path: "/secret".to_string(),
        };
        let mapped = SftpDownloadCommand::map_file_op_error(&err, "host", "/secret");
        assert!(matches!(
            mapped,
            Aria2Error::Fatal(FatalError::PermissionDenied { .. })
        ));
    }

    #[test]
    fn test_map_network_error_to_recoverable() {
        let err = FileOpError::Network {
            operation: "READ".into(),
            message: "Connection reset".into(),
        };
        let mapped = SftpDownloadCommand::map_file_op_error(&err, "host", "/f");
        assert!(matches!(
            mapped,
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
        ));
    }

    #[test]
    fn test_constants() {
        assert_eq!(DEFAULT_CONNECT_TIMEOUT_SECS, 15);
        assert_eq!(DEFAULT_READ_TIMEOUT_SECS, 30);
        assert_eq!(DEFAULT_COMMAND_TIMEOUT_SECS, 300);
        assert_eq!(DISK_WRITE_CHUNK_SIZE, 65536); // 64KB
        assert_eq!(SPEED_UPDATE_INTERVAL_MS, 500);
    }

    /// Helper to create a test command instance
    fn create_test_cmd() -> SftpDownloadCommand {
        SftpDownloadCommand {
            group: Arc::new(tokio::sync::RwLock::new(RequestGroup::new(
                GroupId::new(1),
                vec!["sftp://testuser:secretpass@example.com:2222/path/to/file.zip".to_string()],
                DownloadOptions::default(),
            ))),
            output_path: std::path::PathBuf::from("/tmp/download/file.zip"),
            started: false,
            completed_bytes: 0,
            host: "example.com".to_string(),
            port: 2222,
            username: "testuser".to_string(),
            password: Some("secretpass".to_string()),
            remote_path: "/path/to/file.zip".to_string(),
        }
    }
}
