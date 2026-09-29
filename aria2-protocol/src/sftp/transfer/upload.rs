//! Standalone SFTP upload transfer.

use std::time::UNIX_EPOCH;
use tokio::io::AsyncSeekExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::super::file_ops::{FileAttributes, FileOpError, OpenFlags};
use super::types::{TransferError, TransferOptions, TransferProgress};
use super::{PROGRESS_REPORT_INTERVAL, SftpTransfer};
impl<'a> SftpTransfer<'a> {
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
    ) -> Result<TransferProgress, TransferError> {
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
    ) -> Result<TransferProgress, TransferError> {
        self.upload_controlled(local_path, remote_path, options, Some(cancellation))
            .await
    }

    async fn upload_controlled(
        &self,
        local_path: &std::path::Path,
        remote_path: &str,
        options: &TransferOptions,
        cancellation: Option<&CancellationToken>,
    ) -> Result<TransferProgress, TransferError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(TransferError::Cancelled);
        }

        info!(
            "[SFTP] Upload start: {} -> {}",
            local_path.display(),
            remote_path
        );

        // Get local file size
        let metadata = match tokio::fs::metadata(local_path).await {
            Ok(m) => m,
            Err(error) => return Err(TransferError::local("read metadata for", local_path, error)),
        };
        let total_size = metadata.len();

        // Open remote file for writing
        let mut remote_file = match self
            .ops
            .open(remote_path, OpenFlags::write_create(), 0o644)
            .await
        {
            Ok(f) => f,
            Err(error) => {
                return Err(TransferError::remote(
                    "open for writing",
                    remote_path,
                    error,
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
                Ok(_) | Err(FileOpError::NotFound { .. }) => 0,
                Err(error) => {
                    return Err(TransferError::remote(
                        "inspect for resume",
                        remote_path,
                        error,
                    ));
                }
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
            Err(error) => return Err(TransferError::local("open for reading", local_path, error)),
        };

        // Seek to resume position
        if start_offset > 0
            && let Err(e) = local_file
                .seek(std::io::SeekFrom::Start(start_offset))
                .await
        {
            return Err(TransferError::local(
                format!("seek to resume offset {start_offset}"),
                local_path,
                e,
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
                return Err(TransferError::Cancelled);
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
                Err(error) => {
                    return Err(TransferError::local(
                        format!("read at offset {transferred}"),
                        local_path,
                        error,
                    ));
                }
            };

            if let Err(e) = remote_file.write_at(transferred, &buf[..n]).await {
                return Err(TransferError::remote(
                    format!("write at offset={transferred}, len={n}"),
                    remote_path,
                    e,
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

        // Preserve source permissions and timestamps in one SFTP SETSTAT.
        if options.preserve_permissions || options.preserve_time {
            let mut attrs = FileAttributes::default();

            if options.preserve_permissions {
                #[cfg(unix)]
                {
                    attrs.permissions = metadata.permissions().mode() as u32 & 0o7777;
                }
                #[cfg(not(unix))]
                {
                    attrs.permissions = if metadata.permissions().readonly() {
                        0o444
                    } else {
                        0o644
                    };
                }
            }

            if options.preserve_time {
                attrs.atime = match metadata.accessed() {
                    Ok(time) => time
                        .duration_since(UNIX_EPOCH)
                        .map(|time| time.as_secs().min(u64::from(u32::MAX)) as u32)
                        .unwrap_or(0),
                    Err(error) => {
                        warn!("[SFTP] Failed to read local access time: {}", error);
                        0
                    }
                };
                attrs.mtime = match metadata.modified() {
                    Ok(time) => time
                        .duration_since(UNIX_EPOCH)
                        .map(|time| time.as_secs().min(u64::from(u32::MAX)) as u32)
                        .unwrap_or(0),
                    Err(error) => {
                        warn!("[SFTP] Failed to read local modification time: {}", error);
                        0
                    }
                };
            }

            if let Err(error) = self.ops.set_stat(remote_path, &attrs).await {
                warn!("[SFTP] Failed to preserve remote file metadata: {}", error);
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
}
