//! SFTP download execution logic.
//!
//! Implements the `Command` trait for `SftpDownloadCommand`, orchestrating
//! the full download lifecycle: SSH connect, SFTP session init, stat remote
//! file, chunked read/write transfer, and cleanup/finalize.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

use crate::constants;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

use aria2_protocol::sftp::connection::SshConnection;
use aria2_protocol::sftp::file_ops::{OpenFlags, SftpFileOps};
use aria2_protocol::sftp::session::SftpSession;

use super::types::SftpDownloadCommand;

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
            self.group.recover_mut().start()?;
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

        let session: SftpSession = match session_result {
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
            let g = self.group.recover();
            g.set_total_length(total_length);
        }

        // -----------------------------------------------------------------
        // Phase 4: Open Remote File for Reading
        // -----------------------------------------------------------------
        let remote_file = match ops
            .open(&self.remote_path, OpenFlags::readonly(), 0)
            .await
        {
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
            let g = self.group.recover();
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
        let _buf = vec![0u8; constants::SFTP_DISK_WRITE_CHUNK_SIZE];

        info!("[SFTP-CMD] Starting transfer loop: {} bytes", total_length);

        loop {
            let remaining = total_length.saturating_sub(self.completed_bytes);
            if remaining == 0 {
                break; // Download complete
            }

            // Calculate how much to read this iteration
            let to_read = (constants::SFTP_DISK_WRITE_CHUNK_SIZE as u64).min(remaining) as usize;

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
                let g = self.group.recover();
                g.update_progress(self.completed_bytes);

                // Periodic speed calculation (every ~500ms)
                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= constants::SFTP_SPEED_UPDATE_INTERVAL_MS as u128 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0);
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
            let g = self.group.recover();
            g.update_progress(self.completed_bytes);
            g.update_speed(final_speed, 0);
            drop(g);
            let mut g = self.group.recover_mut();
            g.complete()?;
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

    fn gid(&self) -> GroupId {
        self.group.recover().gid()
    }

    /// Return the timeout for this command.
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(constants::SFTP_COMMAND_TIMEOUT_SECS))
    }
}
