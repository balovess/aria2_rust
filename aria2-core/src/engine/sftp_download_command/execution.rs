//! SFTP download execution logic.
//!
//! Implements the `Command` trait for `SftpDownloadCommand`, orchestrating
//! the full download lifecycle: SSH connect, SFTP session init, stat remote
//! file, chunked read/write transfer, and cleanup/finalize.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

use crate::checksum::checksum::Checksum;
use crate::constants;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DiskWriter, new_sequential_download_writer};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

use aria2_protocol::sftp::connection::SshConnection;
use aria2_protocol::sftp::file_ops::{FileOpError, OpenFlags, SftpFileOps};
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

        let in_memory_download = self.group.recover().is_in_memory_download();

        debug!(
            "[SFTP-CMD] Starting download: {}@{}:{} -> {}",
            self.username,
            self.host,
            self.port,
            self.output_path.display()
        );

        // Ensure output directory exists
        if !in_memory_download
            && let Some(parent) = self.output_path.parent()
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
                let err = FileOpError::from(e);
                return Err(Self::map_file_op_error(&err, &self.host, &self.remote_path));
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

        // Update RequestGroup with total length and recover a local prefix.
        // SFTP reads are positioned, so an existing partial file can be resumed
        // without downloading or rewriting its already-present prefix.
        let existing_length = if in_memory_download {
            0
        } else {
            tokio::fs::metadata(&self.output_path)
                .await
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        };
        let continue_download = self.group.recover().options().continue_download;
        let checksum_config = self.group.recover().options().checksum.clone();
        let complete_local_checksum_candidate =
            !in_memory_download && existing_length == total_length && checksum_config.is_some();
        let resume_input_length = if complete_local_checksum_candidate {
            // aria2_original routes a complete local file with a configured
            // checksum through ChecksumCheckIntegrityEntry even without
            // `--continue`; only a failed check returns to remote download.
            existing_length
        } else {
            crate::engine::progress_checkpoint::ProgressCheckpoint::resume_input_length(
                &self.output_path,
                existing_length,
                continue_download,
                total_length,
            )
            .await
        };
        Self::validate_resume_offset(resume_input_length, total_length)?;
        if in_memory_download {
            self.checkpoint = None;
            self.completed_bytes = 0;
        } else {
            self.checkpoint = Some(
                crate::engine::progress_checkpoint::ProgressCheckpoint::open(
                    &self.output_path,
                    total_length,
                    resume_input_length,
                )
                .await,
            );
            self.completed_bytes = self
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.resume_offset(resume_input_length))
                .unwrap_or(resume_input_length);
        }
        {
            let g = self.group.recover();
            g.set_total_length(total_length);
            g.update_progress(self.completed_bytes);
        }

        if complete_local_checksum_candidate && self.completed_bytes == total_length {
            let (algorithm, expected) = checksum_config
                .as_ref()
                .expect("complete local checksum candidate has a checksum");
            let checksum = Checksum::from_type_and_value(algorithm, expected)?;
            if crate::checksum::checksum::verify_file(&self.output_path, &checksum).await? {
                self.group.recover().set_checksum_verified(true);
                self.group.recover().set_completed_length(total_length);
                self.group.recover_mut().complete()?;
                self.complete_checkpoint().await;
                info!(
                    path = %self.output_path.display(),
                    size = total_length,
                    "SFTP target already matches remote size and checksum"
                );
                let _ = conn.disconnect().await;
                return Ok(());
            }

            warn!(
                path = %self.output_path.display(),
                "SFTP target checksum mismatch; restarting from byte zero"
            );
            self.completed_bytes = 0;
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(0, true).await;
            }
        }

        // -----------------------------------------------------------------
        // Phase 4: Open Remote File for Reading
        // -----------------------------------------------------------------
        let mut remote_file = match ops.open(&self.remote_path, OpenFlags::readonly(), 0).await {
            Ok(f) => f,
            Err(e) => {
                let _ = conn.disconnect().await;
                let err = FileOpError::from(e);
                return Err(Self::map_file_op_error(&err, &self.host, &self.remote_path));
            }
        };

        // -----------------------------------------------------------------
        // Phase 5: Prepare Local Disk Writer
        // -----------------------------------------------------------------
        let raw_writer = new_sequential_download_writer(
            &self.output_path,
            in_memory_download,
            self.completed_bytes,
            Some(total_length),
        );

        // Apply rate limiting if configured
        let rate_limit = {
            let g = self.group.recover();
            g.options().max_download_limit
        };
        // Global (process-wide) limiter: when present and enabled, the writer
        // acquires tokens after the per-download limiter so all concurrent
        // downloads share a single bandwidth ceiling.
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());
        let mut writer: Box<dyn DiskWriter> = if rate_limit.is_some() || global_limited {
            let per_rate = rate_limit.filter(|&r| r > 0);
            let limiter = per_rate
                .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)))
                .unwrap_or_else(RateLimiter::unlimited);
            let mut tw = ThrottledWriter::new(raw_writer, limiter);
            if let Some(ref gl) = self.global_limiter {
                tw = tw.with_global_limiter(gl.clone());
            }
            Box::new(tw)
        } else {
            raw_writer
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
            let halted = {
                let group = self.group.recover();
                group.is_removed() || group.is_force_halt_requested() || group.is_halt_requested()
            };
            if halted {
                let halt_error = {
                    let group = self.group.recover();
                    if group.is_removed() {
                        "Download cancelled by user"
                    } else if group.is_paused_flag() {
                        "Download paused"
                    } else {
                        "SFTP download halted"
                    }
                };
                let _ = remote_file.close().await;
                self.finalize_partial_writer(&mut writer).await;
                self.flush_checkpoint().await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::DownloadFailed(halt_error.into()));
            }

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
                    let _ = remote_file.close().await;
                    self.finalize_partial_writer(&mut writer).await;
                    let _ = conn.disconnect().await;
                    return Err(Aria2Error::DownloadFailed(format!(
                        "SFTP remote file ended before the advertised length: {} < {}",
                        self.completed_bytes, total_length
                    )));
                }
                Ok(data) => data,
                Err(e) => {
                    error!(
                        "[SFTP-CMD] Read error at offset {}: {}",
                        self.completed_bytes, e
                    );
                    let _ = remote_file.close().await;
                    self.finalize_partial_writer(&mut writer).await;
                    let _ = conn.disconnect().await;
                    let err = FileOpError::from(e);
                    return Err(Self::map_file_op_error(&err, &self.host, &self.remote_path));
                }
            };
            let n = data.len();

            // Write chunk to local disk via disk writer
            if let Err(e) = writer.write(&data).await {
                error!("[SFTP-CMD] Disk write error: {}", e);
                let _ = remote_file.close().await;
                self.finalize_partial_writer(&mut writer).await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::Fatal(FatalError::Config(format!(
                    "Disk write failed: {}",
                    e
                ))));
            }

            self.completed_bytes += n as u64;
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(self.completed_bytes, false).await;
            }

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

        // Finalize disk writer (flush, sync, etc.). Completion is not valid
        // unless the local bytes have been durably finalized.
        let finalized_data = match writer.finalize().await {
            Ok(data) => data,
            Err(error) => {
                self.flush_checkpoint().await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::FileIo(format!(
                    "[SFTP-CMD] Failed to finalize disk writer: {error}"
                )));
            }
        };

        // aria2_original schedules ChecksumCheckIntegrityEntry after the
        // SFTP transfer, including when the local output was already complete.
        // Keep that verification at the Rust-owned completion seam so the
        // output is never marked complete before its configured checksum is
        // known to match.
        if let Some((algorithm, expected)) = checksum_config {
            let checksum = match Checksum::from_type_and_value(&algorithm, &expected) {
                Ok(checksum) => checksum,
                Err(error) => {
                    let _ = conn.disconnect().await;
                    return Err(error);
                }
            };
            let verified = if in_memory_download {
                checksum.verify(&finalized_data)
            } else {
                match crate::checksum::checksum::verify_file(&self.output_path, &checksum).await {
                    Ok(verified) => verified,
                    Err(error) => {
                        self.flush_checkpoint().await;
                        let _ = conn.disconnect().await;
                        return Err(error);
                    }
                }
            };
            if !verified {
                self.flush_checkpoint().await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::Checksum(format!(
                    "{} checksum mismatch for {}",
                    algorithm,
                    self.output_path.display()
                )));
            }
            self.group.recover().set_checksum_verified(true);
        }

        if in_memory_download {
            let group = self.group.recover();
            group.set_total_length(self.completed_bytes);
            group.set_completed_length(self.completed_bytes);
            group.set_in_memory_data(finalized_data);
        }

        // Disconnect SSH session
        if let Err(e) = conn.disconnect().await {
            warn!("[SFTP-CMD] Warning during SSH disconnect: {}", e);
        }

        self.complete_checkpoint().await;

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

    fn request_group(
        &self,
    ) -> Option<std::sync::Arc<std::sync::RwLock<crate::request::request_group::RequestGroup>>>
    {
        Some(std::sync::Arc::clone(&self.group))
    }

    /// Return the timeout for this command.
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(constants::SFTP_COMMAND_TIMEOUT_SECS))
    }

    async fn shutdown(&mut self) {
        self.flush_checkpoint().await;
    }
}

impl SftpDownloadCommand {
    async fn finalize_partial_writer(&mut self, writer: &mut Box<dyn DiskWriter>) {
        let _ = writer.finalize().await;
        self.flush_checkpoint().await;
    }

    async fn flush_checkpoint(&mut self) {
        if let Some(checkpoint) = self.checkpoint.as_mut() {
            checkpoint.update(self.completed_bytes, true).await;
        }
    }

    async fn complete_checkpoint(&mut self) {
        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.complete().await;
        }
    }
}
