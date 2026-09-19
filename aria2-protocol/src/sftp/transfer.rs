//! SFTP Transfer Protocol
//!
//! Implements high-level download and upload transfer operations over SFTP,
//! including chunked I/O, resume support, progress tracking, and rate control
//! integration.
//!
//! ## Transfer Architecture
//!
//! ```text
//! TransferOptions  ->  SftpTransfer  ->  [Chunked Read Loop]  ->  Local File
//!       |                  |                    |
//!   buffer_size      file_ops             read_at(offset, buf)
//!   resume_offset                         write(chunk)
//!   progress_cb                            update_progress()
//! ```

use tokio::io::AsyncSeekExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::file_ops::{FileAttributes, OpenFlags, SftpFileOps};
use super::session::SftpSession;

/// Default size for each data chunk transferred (64 KB)
const TRANSFER_BUF_SIZE: usize = 64 * 1024;
/// Minimum allowed buffer size (1 KB)
const MIN_BUFFER_SIZE: usize = 1024;
/// Maximum allowed buffer size (1 MB)
const MAX_BUFFER_SIZE: usize = 1024 * 1024;
/// Interval in bytes between progress reports (256 KB)
const PROGRESS_REPORT_INTERVAL: u64 = 256 * 1024;

// =============================================================================
// Transfer Mode and Options
// =============================================================================

/// Data transfer mode for text/binary handling.
///
/// Note: SFTP always transfers data as binary streams. This enum exists
/// for API consistency with FTP transfer modes but has no effect on
/// the actual wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferMode {
    /// Binary mode - no line ending translation
    #[default]
    Binary,
    /// Text mode (no-op in SFTP, for API consistency)
    Text,
}

/// Progress callback signature: receives (bytes_transferred, total_bytes, speed_bps)
pub type ProgressCallback = Box<dyn Fn(u64, u64, f64) + Send + Sync>;

/// Configuration options for SFTP transfer operations.
pub struct TransferOptions {
    /// Size of each read/write buffer (default: 64KB, range: 1KB-1MB)
    pub buffer_size: usize,
    /// Starting byte offset for resume/partial downloads
    pub resume_offset: u64,
    /// Transfer mode (always binary for SFTP)
    pub mode: TransferMode,
    /// Whether to preserve remote file permissions on local copy
    pub preserve_permissions: bool,
    /// Whether to preserve remote file timestamps on local copy
    pub preserve_time: bool,
    /// Optional progress callback invoked periodically during transfer
    pub progress_callback: Option<ProgressCallback>,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            buffer_size: TRANSFER_BUF_SIZE,
            resume_offset: 0,
            mode: TransferMode::Binary,
            preserve_permissions: false,
            preserve_time: false,
            progress_callback: None,
        }
    }
}

impl TransferOptions {
    /// Set the resume offset for partial download continuation.
    pub fn with_resume(mut self, offset: u64) -> Self {
        self.resume_offset = offset;
        self
    }

    /// Set a custom buffer size (clamped to valid range).
    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size.clamp(MIN_BUFFER_SIZE, MAX_BUFFER_SIZE);
        self
    }

    /// Enable preservation of remote file metadata (permissions + timestamps).
    pub fn preserve_metadata(mut self) -> Self {
        self.preserve_permissions = true;
        self.preserve_time = true;
        self
    }

    /// Register a progress callback that will be called periodically.
    pub fn with_progress_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(u64, u64, f64) + Send + Sync + 'static,
    {
        self.progress_callback = Some(Box::new(cb));
        self
    }
}

// =============================================================================
// Transfer Progress
// =============================================================================

/// Represents the current state of an in-progress or completed transfer.
#[derive(Debug, Clone)]
pub struct TransferProgress {
    /// Total bytes transferred so far
    pub bytes_transferred: u64,
    /// Total expected bytes (file size, 0 if unknown)
    pub total_bytes: u64,
    /// Current transfer speed in bytes per second
    pub speed_bytes_per_sec: f64,
    /// Elapsed time since transfer started (seconds)
    pub elapsed_secs: f64,
}

impl TransferProgress {
    /// Calculate completion percentage (100.0 if total is unknown/zero).
    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            100.0
        } else {
            (self.bytes_transferred as f64 / self.total_bytes as f64) * 100.0
        }
    }

    /// Check if the transfer is complete (transferred >= total or total unknown).
    pub fn is_complete(&self) -> bool {
        self.bytes_transferred >= self.total_bytes || self.total_bytes == 0
    }

    /// Get remaining bytes to transfer.
    pub fn remaining(&self) -> u64 {
        self.total_bytes.saturating_sub(self.bytes_transferred)
    }

    /// Estimated time remaining in seconds (based on current speed).
    pub fn eta_secs(&self) -> Option<f64> {
        let remaining = self.remaining();
        if self.speed_bytes_per_sec > 0.0 && remaining > 0 {
            Some(remaining as f64 / self.speed_bytes_per_sec)
        } else {
            None
        }
    }
}

impl std::fmt::Display for TransferProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.1}% ({}/{} @ {:.1} KB/s, {:.1}s elapsed",
            self.percent(),
            self.bytes_transferred,
            if self.total_bytes > 0 {
                format!("{}", self.total_bytes)
            } else {
                "?".to_string()
            },
            self.speed_bytes_per_sec / 1024.0,
            self.elapsed_secs
        )?;
        if let Some(eta) = self.eta_secs() {
            write!(f, ", ETA: {:.1}s", eta)?;
        }
        Ok(())
    }
}

// =============================================================================
// SFTP Transfer Engine
// =============================================================================

/// High-level SFTP transfer engine for downloads and uploads.
///
/// Provides async methods for transferring files between local disk and
/// remote SFTP server with support for:
/// - Chunked reading/writing with configurable buffer sizes
/// - Resume/partial download support via offset control
/// - Progress tracking with optional callbacks
/// - Metadata preservation (permissions, timestamps)
///
/// # Example
///
/// ```ignore
/// let session = SftpSession::open(&conn).await?;
/// let transfer = SftpTransfer::new(&session);
///
/// let options = TransferOptions::default()
///     .with_buffer_size(128 * 1024)
///     .with_resume(saved_offset);
///
/// let progress = transfer.download("/remote/file.bin", &local_path, &options).await?;
/// println!("Downloaded: {}", progress);
/// ```
pub struct SftpTransfer<'a> {
    /// File operations interface bound to this session
    ops: SftpFileOps<'a>,
}

impl<'a> SftpTransfer<'a> {
    /// Create a new transfer engine bound to the given SFTP session.
    pub fn new(session: &'a SftpSession) -> Self {
        Self {
            ops: SftpFileOps::new(session),
        }
    }

    /// Get the underlying file operations interface.
    pub fn ops(&self) -> &SftpFileOps<'a> {
        &self.ops
    }

    // -----------------------------------------------------------------
    // Download Operation
    // -----------------------------------------------------------------

    /// Download a remote file to a local path with full progress tracking.
    ///
    /// # Protocol Flow
    /// ```text
    /// LSTAT(remote_path)          -- verify it is a regular file, get size
    /// OPEN(remote_path, READ)     -- get file handle
    /// CREATE/TRUNCATE(local_path) -- prepare local file (or seek for resume)
    /// LOOP:
    ///   READ(handle, offset, buf) -- read up to buffer_size bytes
    ///   WRITE(local_file, buf)    -- write to local disk
    ///   UPDATE_PROGRESS           -- track transferred bytes
    /// UNTIL EOF || error
    /// CLOSE(handle)               -- release remote handle
    /// ```
    ///
    /// # Arguments
    /// * `remote_path` - Path to the file on the SFTP server
    /// * `local_path` - Destination path on local filesystem
    /// * `options` - Transfer configuration (buffer size, resume offset, etc.)
    ///
    /// # Returns
    /// Final `TransferProgress` indicating completion status and statistics.
    pub async fn download(
        &self,
        remote_path: &str,
        local_path: &std::path::Path,
        options: &TransferOptions,
    ) -> Result<TransferProgress, String> {
        self.download_controlled(remote_path, local_path, options, None)
            .await
    }

    /// Download a remote file while observing a cancellation token.
    ///
    /// Cancellation is checked between remote read/write iterations. The
    /// remote handle is closed on the cancellation path when possible, and a
    /// stable cancellation error is returned to the caller.
    pub async fn download_with_cancellation(
        &self,
        remote_path: &str,
        local_path: &std::path::Path,
        options: &TransferOptions,
        cancellation: &CancellationToken,
    ) -> Result<TransferProgress, String> {
        self.download_controlled(remote_path, local_path, options, Some(cancellation))
            .await
    }

    async fn download_controlled(
        &self,
        remote_path: &str,
        local_path: &std::path::Path,
        options: &TransferOptions,
        cancellation: Option<&CancellationToken>,
    ) -> Result<TransferProgress, String> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("SFTP download cancelled".to_string());
        }

        info!(
            "[SFTP] Download start: {} -> {}",
            remote_path,
            local_path.display()
        );

        // Step 1: Stat the remote file to verify it exists and get its size
        let remote_attr = match self.ops.lstat(remote_path).await {
            Ok(attr) => attr,
            Err(e) => {
                return Err(format!("Cannot stat remote file [{}]: {}", remote_path, e));
            }
        };

        if !remote_attr.is_regular_file {
            return Err(format!(
                "Remote path is not a regular file: {} (type={})",
                remote_path,
                if remote_attr.is_directory {
                    "directory"
                } else if remote_attr.is_symlink {
                    "symlink"
                } else {
                    "unknown"
                }
            ));
        }

        let total_size = remote_attr.size;

        // Step 2: Calculate effective start offset (for resume support)
        let start_offset = if options.resume_offset > 0 {
            options.resume_offset.min(total_size)
        } else {
            0
        };

        debug!(
            "[SFTP] Remote file size: {}, start offset: {}, to-transfer: {}",
            total_size,
            start_offset,
            total_size.saturating_sub(start_offset)
        );

        // Step 3: Open remote file for reading
        let mut remote_file = match self.ops.open(remote_path, OpenFlags::readonly(), 0).await {
            Ok(f) => f,
            Err(e) => {
                return Err(format!(
                    "Failed to open remote file [{}]: {}",
                    remote_path, e
                ));
            }
        };

        // Step 4: Prepare local file (create or seek for resume)
        let mut local_file = if start_offset > 0 && local_path.exists() {
            // Resume mode: open existing file and seek to offset
            match tokio::fs::OpenOptions::new()
                .write(true)
                .open(local_path)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    return Err(format!(
                        "Failed to open local file for resume [{}]: {}",
                        local_path.display(),
                        e
                    ));
                }
            }
        } else {
            // Fresh download: create/truncate local file
            match tokio::fs::File::create(local_path).await {
                Ok(f) => f,
                Err(e) => {
                    return Err(format!(
                        "Failed to create local file [{}]: {}",
                        local_path.display(),
                        e
                    ));
                }
            }
        };

        // Seek to resume position if applicable
        if start_offset > 0 {
            if let Err(e) = local_file.set_len(start_offset).await {
                warn!("[SFTP] Could not set file length for resume: {}", e);
            }
            if let Err(e) = local_file
                .seek(std::io::SeekFrom::Start(start_offset))
                .await
            {
                return Err(format!(
                    "Failed to seek to resume position {}: {}",
                    start_offset, e
                ));
            }
            info!("[SFTP] Resuming from offset {}", start_offset);
        }

        // Step 5: Main transfer loop
        let mut transferred = start_offset;
        let start_time = std::time::Instant::now();
        let mut last_report = start_offset;

        loop {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                let _ = remote_file.close().await;
                return Err("SFTP download cancelled".to_string());
            }

            let remaining = total_size.saturating_sub(transferred);
            if remaining == 0 {
                break; // Transfer complete
            }

            // Calculate how much to read this iteration
            let to_read = (options.buffer_size as u64).min(remaining) as usize;

            // Read chunk from remote file at current offset
            let data = match remote_file.read_at(transferred, to_read as u32).await {
                Ok(data) if data.is_empty() => {
                    debug!("[SFTP] EOF reached at offset {}", transferred);
                    break; // Server returned empty data (EOF)
                }
                Ok(data) => data,
                Err(e) => {
                    return Err(format!(
                        "Read failed at offset={} (remaining={}): {}",
                        transferred, remaining, e
                    ));
                }
            };

            let n = data.len();

            // Write chunk to local file
            if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut local_file, &data).await {
                return Err(format!(
                    "Write to local file failed at offset {}: {}",
                    transferred, e
                ));
            }

            transferred += n as u64;

            // Progress reporting (throttled to avoid excessive logging/callbacks)
            if transferred.saturating_sub(last_report) >= PROGRESS_REPORT_INTERVAL {
                last_report = transferred;
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    (transferred - start_offset) as f64 / elapsed
                } else {
                    0.0
                };
                debug!(
                    "[SFTP] Progress: {:.1}% ({}/{}, {:.1} KB/s)",
                    (transferred as f64 / total_size as f64) * 100.0,
                    transferred,
                    total_size,
                    speed / 1024.0
                );

                // Invoke user-provided progress callback if set
                if let Some(ref cb) = options.progress_callback {
                    cb(transferred, total_size, speed);
                }
            }
        }

        // Step 6: Cleanup
        if let Err(e) = remote_file.close().await {
            warn!("[SFTP] Error closing remote file handle: {}", e);
        }
        drop(local_file);

        // Calculate final statistics
        let elapsed = start_time.elapsed().as_secs_f64();
        let avg_speed = if elapsed > 0.0 {
            (transferred - start_offset) as f64 / elapsed
        } else {
            0.0
        };

        let progress = TransferProgress {
            bytes_transferred: transferred,
            total_bytes: total_size,
            speed_bytes_per_sec: avg_speed,
            elapsed_secs: elapsed,
        };

        info!(
            "[SFTP] Download complete: {}, {:.1} KB/s, {:.1}s",
            progress,
            avg_speed / 1024.0,
            elapsed
        );

        Ok(progress)
    }

    // -----------------------------------------------------------------
    // Upload Operation
    // -----------------------------------------------------------------

    /// Upload a local file to a remote SFTP path.
    ///
    /// # Arguments
    /// * `local_path` - Source file on local filesystem
    /// * `remote_path` - Destination path on SFTP server
    /// * `options` - Transfer configuration
    pub async fn upload(
        &self,
        local_path: &std::path::Path,
        remote_path: &str,
        options: &TransferOptions,
    ) -> Result<TransferProgress, String> {
        self.upload_controlled(local_path, remote_path, options, None)
            .await
    }

    /// Upload a local file while observing a cancellation token.
    pub async fn upload_with_cancellation(
        &self,
        local_path: &std::path::Path,
        remote_path: &str,
        options: &TransferOptions,
        cancellation: &CancellationToken,
    ) -> Result<TransferProgress, String> {
        self.upload_controlled(local_path, remote_path, options, Some(cancellation))
            .await
    }

    async fn upload_controlled(
        &self,
        local_path: &std::path::Path,
        remote_path: &str,
        options: &TransferOptions,
        cancellation: Option<&CancellationToken>,
    ) -> Result<TransferProgress, String> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("SFTP upload cancelled".to_string());
        }

        info!(
            "[SFTP] Upload start: {} -> {}",
            local_path.display(),
            remote_path
        );

        // Get local file size
        let metadata = match tokio::fs::metadata(local_path).await {
            Ok(m) => m,
            Err(e) => {
                return Err(format!(
                    "Cannot get local file metadata [{}]: {}",
                    local_path.display(),
                    e
                ));
            }
        };
        let total_size = metadata.len();

        // Open remote file for writing
        let mut remote_file = match self
            .ops
            .open(remote_path, OpenFlags::write_create(), 0o644)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                return Err(format!(
                    "Failed to open remote file for writing [{}]: {}",
                    remote_path, e
                ));
            }
        };

        // Determine starting offset (resume support)
        let start_offset = if options.resume_offset > 0 {
            options.resume_offset.min(total_size)
        } else if options.resume_offset == 0 {
            // Check if remote file already exists (for auto-resume)
            match self.ops.lstat(remote_path).await {
                Ok(attr) if attr.is_regular_file => attr.size.min(total_size),
                _ => 0,
            }
        } else {
            0
        };

        if start_offset > 0 {
            debug!("[SFTP] Upload resume mode, start offset: {}", start_offset);
        }

        // Open local file for reading
        let mut local_file = match tokio::fs::File::open(local_path).await {
            Ok(f) => f,
            Err(e) => {
                return Err(format!(
                    "Failed to open local file [{}]: {}",
                    local_path.display(),
                    e
                ));
            }
        };

        // Seek to resume position
        if start_offset > 0
            && let Err(e) = local_file
                .seek(std::io::SeekFrom::Start(start_offset))
                .await
        {
            return Err(format!(
                "Failed to seek to upload resume position {}: {}",
                start_offset, e
            ));
        }

        // Upload loop
        let mut buf = vec![0u8; options.buffer_size];
        let mut transferred = start_offset;
        let start_time = std::time::Instant::now();
        let mut last_report = start_offset;

        loop {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                let _ = remote_file.close().await;
                return Err("SFTP upload cancelled".to_string());
            }

            let remaining = total_size.saturating_sub(transferred);
            if remaining == 0 {
                break;
            }

            let to_read = (options.buffer_size as u64).min(remaining) as usize;

            let n = match tokio::io::AsyncReadExt::read(&mut local_file, &mut buf[..to_read]).await
            {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    return Err(format!("Read local file failed: {}", e));
                }
            };

            if let Err(e) = remote_file.write_at(transferred, &buf[..n]).await {
                return Err(format!(
                    "Write to remote file failed at offset={}, len={}: {}",
                    transferred, n, e
                ));
            }

            transferred += n as u64;

            // Progress reporting
            if transferred.saturating_sub(last_report) >= PROGRESS_REPORT_INTERVAL {
                last_report = transferred;
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    (transferred - start_offset) as f64 / elapsed
                } else {
                    0.0
                };
                debug!(
                    "[SFTP] Upload progress: {:.1}% ({}/{}, {:.1} KB/s)",
                    (transferred as f64 / total_size as f64) * 100.0,
                    transferred,
                    total_size,
                    speed / 1024.0
                );

                if let Some(ref cb) = options.progress_callback {
                    cb(transferred, total_size, speed);
                }
            }
        }

        // Finalize
        // Note: SFTP v3 does not have a native fsync operation (fsync@openssh.com is an extension)
        if let Err(e) = remote_file.close().await {
            warn!("[SFTP] Error closing remote file: {}", e);
        }
        drop(local_file);

        // Preserve permissions if requested
        if options.preserve_permissions {
            #[cfg(unix)]
            let perm = metadata.permissions().mode() as u32;
            #[cfg(not(unix))]
            let perm = 0o644u32;

            if let Err(e) = self
                .ops
                .set_stat(
                    remote_path,
                    &FileAttributes {
                        permissions: perm,
                        ..Default::default()
                    },
                )
                .await
            {
                warn!("[SFTP] Failed to set remote file permissions: {}", e);
            }
        }

        let elapsed = start_time.elapsed().as_secs_f64();
        let avg_speed = if elapsed > 0.0 {
            (transferred - start_offset) as f64 / elapsed
        } else {
            0.0
        };

        let progress = TransferProgress {
            bytes_transferred: transferred,
            total_bytes: total_size,
            speed_bytes_per_sec: avg_speed,
            elapsed_secs: elapsed,
        };

        info!(
            "[SFTP] Upload complete: {}, {:.1} KB/s, {:.1}s",
            progress,
            avg_speed / 1024.0,
            elapsed
        );

        Ok(progress)
    }

    // -----------------------------------------------------------------
    // Utility Methods
    // -----------------------------------------------------------------

    /// Get the size of a remote file without downloading it.
    pub async fn get_remote_size(&self, remote_path: &str) -> Result<u64, String> {
        let attr = self
            .ops
            .stat(remote_path)
            .await
            .map_err(|e| format!("Failed to stat remote file [{}]: {}", remote_path, e))?;
        Ok(attr.size)
    }

    /// Check whether a remote file supports resume (exists and has known size).
    pub async fn check_resume_support(&self, remote_path: &str) -> Result<Option<u64>, String> {
        match self.ops.stat(remote_path).await {
            Ok(attr) if attr.is_regular_file => Ok(Some(attr.size)),
            Ok(_) => Ok(None),  // Exists but not a regular file
            Err(_) => Ok(None), // Doesn't exist
        }
    }

    /// Calculate optimal resume offset given a desired offset and actual file size.
    ///
    /// Ensures the offset doesn't exceed the file size and handles edge cases.
    pub fn calculate_resume_offset(desired_offset: u64, file_size: u64) -> u64 {
        desired_offset.min(file_size)
    }
}

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sftp")]
    mod integration {
        use super::*;
        use crate::sftp::connection::{HostKeyCheckingMode, SshConnection, SshOptions};
        use crate::sftp::packet::{SSH_FX_OK, SftpFileAttrs, SftpPacket};
        use crate::sftp::session::SftpSession;
        use russh::server::{self, Auth, Msg, Server as _, Session};
        use russh::{Channel, ChannelId};
        use std::collections::HashMap;
        use std::net::SocketAddr;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::{Mutex, oneshot};
        use tokio::time::timeout;
        use tokio_util::sync::CancellationToken;

        const REMOTE_FILE_SIZE: u64 = 1_000_000;
        const REMOTE_HANDLE: &[u8] = b"cancellation-test-handle";

        struct TestRng;

        impl russh::keys::ssh_key::rand_core::TryRng for TestRng {
            type Error = std::convert::Infallible;

            fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
                let mut bytes = [0; 4];
                self.try_fill_bytes(&mut bytes)?;
                Ok(u32::from_ne_bytes(bytes))
            }

            fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
                let mut bytes = [0; 8];
                self.try_fill_bytes(&mut bytes)?;
                Ok(u64::from_ne_bytes(bytes))
            }

            fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
                getrandom::getrandom(dest).expect("OS random source failed");
                Ok(())
            }
        }

        impl russh::keys::ssh_key::rand_core::TryCryptoRng for TestRng {}

        #[derive(Clone)]
        struct TestSshServer {
            read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
            close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        }

        struct TestSshSession {
            channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
            read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
            close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        }

        impl server::Server for TestSshServer {
            type Handler = TestSshSession;

            fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
                TestSshSession {
                    channels: Arc::new(Mutex::new(HashMap::new())),
                    read_started: Arc::clone(&self.read_started),
                    close_seen: Arc::clone(&self.close_seen),
                }
            }
        }

        impl TestSshSession {
            async fn take_channel(&self, channel_id: ChannelId) -> Channel<Msg> {
                self.channels
                    .lock()
                    .await
                    .remove(&channel_id)
                    .expect("SFTP subsystem channel was not registered")
            }
        }

        impl server::Handler for TestSshSession {
            type Error = anyhow::Error;

            async fn auth_password(
                &mut self,
                _user: &str,
                _password: &str,
            ) -> Result<Auth, Self::Error> {
                Ok(Auth::Accept)
            }

            async fn channel_open_session(
                &mut self,
                channel: Channel<Msg>,
                _session: &mut Session,
            ) -> Result<bool, Self::Error> {
                self.channels.lock().await.insert(channel.id(), channel);
                Ok(true)
            }

            async fn subsystem_request(
                &mut self,
                channel_id: ChannelId,
                name: &str,
                session: &mut Session,
            ) -> Result<(), Self::Error> {
                if name != "sftp" {
                    session.channel_failure(channel_id)?;
                    return Ok(());
                }

                let channel = self.take_channel(channel_id).await;
                session.channel_success(channel_id)?;

                let read_started = Arc::clone(&self.read_started);
                let close_seen = Arc::clone(&self.close_seen);
                tokio::spawn(async move {
                    let _ = run_test_sftp(channel.into_stream(), read_started, close_seen).await;
                });

                Ok(())
            }
        }

        async fn read_test_packet<S>(
            stream: &mut S,
            buffer: &mut Vec<u8>,
        ) -> std::io::Result<SftpPacket>
        where
            S: AsyncRead + Unpin,
        {
            loop {
                match SftpPacket::decode(buffer) {
                    Ok((packet, consumed)) => {
                        buffer.drain(..consumed);
                        return Ok(packet);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
                    Err(error) => return Err(error),
                }

                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).await?;
                if count == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "SFTP test channel closed",
                    ));
                }
                buffer.extend_from_slice(&chunk[..count]);
            }
        }

        async fn write_test_packet<S>(stream: &mut S, packet: &SftpPacket) -> std::io::Result<()>
        where
            S: AsyncWrite + Unpin,
        {
            stream.write_all(&packet.encode()?).await?;
            stream.flush().await
        }

        async fn run_test_sftp<S>(
            mut stream: S,
            read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
            close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        ) -> std::io::Result<()>
        where
            S: AsyncRead + AsyncWrite + Unpin,
        {
            let mut buffer = Vec::new();
            match read_test_packet(&mut stream, &mut buffer).await? {
                SftpPacket::Init { version } => {
                    assert_eq!(version, 3);
                }
                packet => panic!("expected SFTP INIT, got {packet:?}"),
            }
            write_test_packet(
                &mut stream,
                &SftpPacket::Version {
                    version: 3,
                    extensions: Vec::new(),
                },
            )
            .await?;

            loop {
                let packet = read_test_packet(&mut stream, &mut buffer).await?;
                match packet {
                    SftpPacket::Lstat { request_id, .. } => {
                        write_test_packet(
                            &mut stream,
                            &SftpPacket::Attrs {
                                request_id,
                                attrs: SftpFileAttrs::full(REMOTE_FILE_SIZE, 0, 0, 0o100644, 0, 0),
                            },
                        )
                        .await?;
                    }
                    SftpPacket::Open { request_id, .. } => {
                        write_test_packet(
                            &mut stream,
                            &SftpPacket::Handle {
                                request_id,
                                handle: REMOTE_HANDLE.to_vec(),
                            },
                        )
                        .await?;
                    }
                    SftpPacket::Read { request_id, .. } => {
                        write_test_packet(
                            &mut stream,
                            &SftpPacket::Data {
                                request_id,
                                data: b"partial-data".to_vec(),
                            },
                        )
                        .await?;

                        if let Some(sender) = read_started.lock().await.take() {
                            let _ = sender.send(());
                        }
                    }
                    SftpPacket::Close { request_id, .. } => {
                        write_test_packet(
                            &mut stream,
                            &SftpPacket::Status {
                                request_id,
                                code: SSH_FX_OK,
                                message: "ok".to_string(),
                                language: "en".to_string(),
                            },
                        )
                        .await?;

                        if let Some(sender) = close_seen.lock().await.take() {
                            let _ = sender.send(());
                        }
                        return Ok(());
                    }
                    packet => panic!("unexpected SFTP packet: {packet:?}"),
                }
            }
        }

        #[tokio::test]
        async fn cancellation_closes_an_active_sftp_download() {
            let (read_started_sender, read_started_receiver) = oneshot::channel();
            let (close_seen_sender, close_seen_receiver) = oneshot::channel();
            let read_started = Arc::new(Mutex::new(Some(read_started_sender)));
            let close_seen = Arc::new(Mutex::new(Some(close_seen_sender)));

            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let mut server = TestSshServer {
                read_started: Arc::clone(&read_started),
                close_seen: Arc::clone(&close_seen),
            };
            let mut rng = TestRng;
            let mut config = server::Config::default();
            config.keys.push(
                russh::keys::PrivateKey::random(&mut rng, russh::keys::ssh_key::Algorithm::Ed25519)
                    .unwrap(),
            );
            config.auth_rejection_time_initial = Some(Duration::from_millis(1));
            let config = Arc::new(config);

            let server_task = tokio::spawn(async move {
                let (socket, peer_addr) = listener.accept().await.unwrap();
                let handler = server.new_client(Some(peer_addr));
                let running = server::run_stream(config, socket, handler).await.unwrap();
                let _ = running.await;
            });

            let cancellation = CancellationToken::new();
            let client_cancellation = cancellation.clone();
            let local_path = std::path::PathBuf::from("target")
                .join(format!("sftp-cancellation-{}.bin", std::process::id()));
            tokio::fs::create_dir_all("target")
                .await
                .expect("failed to create the Cargo test output directory");
            let options = SshOptions::new("127.0.0.1", "test-user")
                .with_port(port)
                .with_password("test-password")
                .with_host_key_mode(HostKeyCheckingMode::Disable)
                .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));

            let client_local_path = local_path.clone();
            let client_task = tokio::spawn(async move {
                let mut connection = SshConnection::connect(options).await.map_err(|error| {
                    format!("failed to connect to in-process SFTP server: {error}")
                })?;
                let session = SftpSession::open(&mut connection).await?;
                SftpTransfer::new(&session)
                    .download_with_cancellation(
                        "/remote/cancellation.bin",
                        &client_local_path,
                        &TransferOptions::default(),
                        &client_cancellation,
                    )
                    .await
            });

            timeout(Duration::from_secs(5), read_started_receiver)
                .await
                .expect("SFTP server did not reach the active READ")
                .expect("SFTP server readiness signal was dropped");
            cancellation.cancel();

            let result = timeout(Duration::from_secs(5), client_task)
                .await
                .expect("cancellable SFTP download did not finish")
                .expect("cancellable SFTP client task panicked");
            assert_eq!(result.unwrap_err(), "SFTP download cancelled");

            timeout(Duration::from_secs(5), close_seen_receiver)
                .await
                .expect("SFTP client did not close the remote handle")
                .expect("SFTP close signal was dropped");

            let _ = tokio::fs::remove_file(&local_path).await;
            server_task.abort();
            let _ = server_task.await;
        }
    }

    #[test]
    fn test_transfer_options_defaults() {
        let opts = TransferOptions::default();
        assert_eq!(opts.buffer_size, TRANSFER_BUF_SIZE);
        assert_eq!(opts.resume_offset, 0);
        assert!(matches!(opts.mode, TransferMode::Binary));
        assert!(!opts.preserve_permissions);
        assert!(!opts.preserve_time);
        assert!(opts.progress_callback.is_none());
    }

    #[test]
    fn test_transfer_options_builder() {
        let opts = TransferOptions::default()
            .with_resume(4096)
            .with_buffer_size(128 * 1024)
            .preserve_metadata();

        assert_eq!(opts.resume_offset, 4096);
        assert_eq!(opts.buffer_size, 131072); // 128KB
        assert!(opts.preserve_permissions);
        assert!(opts.preserve_time);
    }

    #[test]
    fn test_transfer_options_buffer_clamp() {
        let small = TransferOptions::default().with_buffer_size(512); // Below min
        assert_eq!(small.buffer_size, MIN_BUFFER_SIZE);

        let large = TransferOptions::default().with_buffer_size(2048 * 1024); // Above max
        assert_eq!(large.buffer_size, MAX_BUFFER_SIZE);

        let exact = TransferOptions::default().with_buffer_size(32768); // Valid
        assert_eq!(exact.buffer_size, 32768);
    }

    #[test]
    fn test_transfer_options_progress_callback() {
        let callback_invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = callback_invoked.clone();

        let opts = TransferOptions::default().with_progress_callback(move |_, _, _| {
            flag_clone.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        if let Some(ref cb) = opts.progress_callback {
            cb(1000, 5000, 1024.0);
        }
        assert!(callback_invoked.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_progress_percent_zero_total() {
        let prog = TransferProgress {
            bytes_transferred: 0,
            total_bytes: 0,
            speed_bytes_per_sec: 0.0,
            elapsed_secs: 0.0,
        };
        assert!((prog.percent() - 100.0).abs() < 0.001);
        assert!(prog.is_complete());
        assert_eq!(prog.remaining(), 0);
    }

    #[test]
    fn test_progress_partial() {
        let prog = TransferProgress {
            bytes_transferred: 500,
            total_bytes: 1000,
            speed_bytes_per_sec: 250.0,
            elapsed_secs: 2.0,
        };
        assert!((prog.percent() - 50.0).abs() < 0.01);
        assert!(!prog.is_complete());
        assert_eq!(prog.remaining(), 500);

        let eta = prog.eta_secs();
        assert!(eta.is_some());
        assert!((eta.unwrap() - 2.0).abs() < 0.01);
    }

    #[test]
    fn test_progress_complete() {
        let prog = TransferProgress {
            bytes_transferred: 1000,
            total_bytes: 1000,
            speed_bytes_per_sec: 500.0,
            elapsed_secs: 2.0,
        };
        assert!((prog.percent() - 100.0).abs() < 0.01);
        assert!(prog.is_complete());
        assert_eq!(prog.remaining(), 0);
        assert!(prog.eta_secs().is_none()); // No remaining = no ETA
    }

    #[test]
    fn test_progress_display_format() {
        let prog = TransferProgress {
            bytes_transferred: 524288,     // 512KB
            total_bytes: 1048576,          // 1MB
            speed_bytes_per_sec: 262144.0, // 256KB/s
            elapsed_secs: 2.0,
        };
        let display = format!("{}", prog);
        assert!(display.contains("50.0%")); // ~50%
        assert!(display.contains("524288"));
        assert!(display.contains("256")); // KB/s
        assert!(display.contains("ETA"));
    }

    #[test]
    fn test_progress_eta_with_zero_speed() {
        let prog = TransferProgress {
            bytes_transferred: 100,
            total_bytes: 10000,
            speed_bytes_per_sec: 0.0,
            elapsed_secs: 10.0,
        };
        assert!(prog.eta_secs().is_none()); // Cannot calculate ETA with zero speed
    }

    #[test]
    fn test_transfer_mode_variants() {
        assert!(matches!(TransferMode::default(), TransferMode::Binary));
        let modes = [TransferMode::Binary, TransferMode::Text];
        for m in &modes {
            let _ = format!("{:?}", m);
        }
    }

    #[test]
    fn test_constants() {
        assert_eq!(TRANSFER_BUF_SIZE, 65536); // 64KB
        assert_eq!(PROGRESS_REPORT_INTERVAL, 262144); // 256KB
        assert_eq!(MIN_BUFFER_SIZE, 1024); // 1KB
        assert_eq!(MAX_BUFFER_SIZE, 1048576); // 1MB
    }

    #[test]
    fn test_calculate_resume_offset() {
        assert_eq!(SftpTransfer::calculate_resume_offset(0, 1000), 0);
        assert_eq!(SftpTransfer::calculate_resume_offset(500, 1000), 500);
        assert_eq!(SftpTransfer::calculate_resume_offset(2000, 1000), 1000); // Clamped
        assert_eq!(SftpTransfer::calculate_resume_offset(999, 1000), 999);
    }
}
