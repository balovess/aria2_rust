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
use tracing::{debug, info, warn};

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
/// for API compatibility with FTP transfer modes but has no effect on
/// the actual wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferMode {
    /// Binary mode - no line ending translation
    #[default]
    Binary,
    /// Text mode (no-op in SFTP, preserved for compatibility)
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
    /// ```
    /// LSTAT(remote_path)          -- verify it's a regular file, get size
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
        let remote_file = match self.ops.open(remote_path, OpenFlags::readonly(), 0).await {
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
        let _buf = vec![0u8; options.buffer_size];
        let mut transferred = start_offset;
        let start_time = std::time::Instant::now();
        let mut last_report = start_offset;

        loop {
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
        let remote_file = match self
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
