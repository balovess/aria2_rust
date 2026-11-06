//! Standalone SFTP download transfer.

use std::time::{Duration, UNIX_EPOCH};

use tokio::io::AsyncSeekExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::super::file_ops::OpenFlags;
use super::types::{TransferError, TransferOptions, TransferProgress};
use super::{PROGRESS_REPORT_INTERVAL, SftpTransfer};
impl<'a> SftpTransfer<'a> {
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
    ) -> Result<TransferProgress, TransferError> {
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
    ) -> Result<TransferProgress, TransferError> {
        self.download_controlled(remote_path, local_path, options, Some(cancellation))
            .await
    }

    async fn download_controlled(
        &self,
        remote_path: &str,
        local_path: &std::path::Path,
        options: &TransferOptions,
        cancellation: Option<&CancellationToken>,
    ) -> Result<TransferProgress, TransferError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(TransferError::Cancelled);
        }

        info!(
            "[SFTP] Download start: {} -> {}",
            remote_path,
            local_path.display()
        );

        // Step 1: Stat the remote file to verify it exists and get its size
        let remote_attr = self
            .ops
            .lstat(remote_path)
            .await
            .map_err(|error| TransferError::remote("lstat", remote_path, error))?;

        if !remote_attr.is_regular_file {
            let file_type = if remote_attr.is_directory {
                "directory"
            } else if remote_attr.is_symlink {
                "symlink"
            } else {
                "unknown"
            };
            return Err(TransferError::NotRegularFile {
                path: remote_path.to_string(),
                file_type,
            });
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
        let mut remote_file = self
            .ops
            .open(remote_path, OpenFlags::readonly(), 0)
            .await
            .map_err(|error| TransferError::remote("open for reading", remote_path, error))?;

        // Step 4: Prepare local file (create or seek for resume)
        let mut local_file = if start_offset > 0 && local_path.exists() {
            // Resume mode: open existing file and seek to offset
            match tokio::fs::OpenOptions::new()
                .write(true)
                .open(local_path)
                .await
            {
                Ok(f) => f,
                Err(error) => {
                    return Err(TransferError::local("open for resume", local_path, error));
                }
            }
        } else {
            // Fresh download: create/truncate local file
            match tokio::fs::File::create(local_path).await {
                Ok(f) => f,
                Err(error) => return Err(TransferError::local("create", local_path, error)),
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
                return Err(TransferError::local(
                    format!("seek to resume offset {start_offset}"),
                    local_path,
                    e,
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
                return Err(TransferError::Cancelled);
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
                Err(error) => {
                    return Err(TransferError::remote(
                        format!("read at offset={transferred}, remaining={remaining}"),
                        remote_path,
                        error,
                    ));
                }
            };

            let n = data.len();

            // Write chunk to local file
            if let Err(error) = tokio::io::AsyncWriteExt::write_all(&mut local_file, &data).await {
                return Err(TransferError::local(
                    format!("write at offset {transferred}"),
                    local_path,
                    error,
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
        let local_file = local_file.into_std().await;

        if options.preserve_time && (remote_attr.atime != 0 || remote_attr.mtime != 0) {
            let mut times = std::fs::FileTimes::new();
            if remote_attr.atime != 0 {
                times = times
                    .set_accessed(UNIX_EPOCH + Duration::from_secs(u64::from(remote_attr.atime)));
            }
            if remote_attr.mtime != 0 {
                times = times
                    .set_modified(UNIX_EPOCH + Duration::from_secs(u64::from(remote_attr.mtime)));
            }
            if let Err(error) = local_file.set_times(times) {
                warn!("[SFTP] Failed to preserve local file timestamps: {}", error);
            }
        }

        if options.preserve_permissions && remote_attr.permissions != 0 {
            #[cfg(unix)]
            {
                let permissions = std::fs::Permissions::from_mode(remote_attr.permissions & 0o7777);
                if let Err(error) = local_file.set_permissions(permissions) {
                    warn!(
                        "[SFTP] Failed to preserve local file permissions: {}",
                        error
                    );
                }
            }
            #[cfg(not(unix))]
            {
                match local_file.metadata() {
                    Ok(metadata) => {
                        let mut permissions = metadata.permissions();
                        permissions.set_readonly(remote_attr.permissions & 0o222 == 0);
                        if let Err(error) = local_file.set_permissions(permissions) {
                            warn!(
                                "[SFTP] Failed to preserve local file permissions: {}",
                                error
                            );
                        }
                    }
                    Err(error) => {
                        warn!(
                            "[SFTP] Failed to read local permissions for preservation: {}",
                            error
                        );
                    }
                }
            }
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
}
