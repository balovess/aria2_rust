//! FTP data-plane transfer, checksum verification, and finalization.

use std::time::{Instant, SystemTime};

use tokio::io::AsyncReadExt;
use tracing::{debug, info};

use crate::checksum::checksum::Checksum;
use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError};
use crate::filesystem::disk_writer::{DiskWriter, new_sequential_download_writer};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::ftp::tls::FtpDataStream;

use super::control::RawFtpControl;
use super::execution::FtpAttemptError;
use super::types::FtpDownloadCommand;

impl FtpDownloadCommand {
    pub(super) async fn receive_data_transfer(
        &mut self,
        mut ctrl: RawFtpControl,
        mut data_stream: FtpDataStream,
        file_size: Option<u64>,
        in_memory_download: bool,
        remote_modified_time: Option<SystemTime>,
        write_offset: u64,
    ) -> std::result::Result<(), FtpAttemptError> {
        // Step 11: Select a disk or memory writer, then apply optional rate
        // limiting. The memory writer is the FTP/SFTP equivalent of aria2's
        // MemoryPreDownloadHandler and never opens output_path.
        let raw_writer = new_sequential_download_writer(
            &self.output_path,
            in_memory_download,
            write_offset,
            file_size,
        );
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
                .map(|rate| {
                    debug!("Rate limiting enabled: {} bytes/sec", rate);
                    RateLimiter::new(&RateLimiterConfig::new(Some(rate), None))
                })
                .unwrap_or_else(RateLimiter::unlimited);
            let mut tw = ThrottledWriter::new(raw_writer, limiter);
            if let Some(ref gl) = self.global_limiter {
                tw = tw.with_global_limiter(gl.clone());
            }
            Box::new(tw)
        } else {
            raw_writer
        };

        // Seek to resume offset if resuming
        // Note: DiskWriter trait doesn't support seek, so for resume we rely on
        // the FTP REST command to tell server to start from the offset,
        // and data will be appended to existing file if it exists
        if write_offset > 0 {
            debug!(
                "Resume offset: {} bytes (using FTP REST command)",
                write_offset
            );
        }

        // Step 12: Data receive loop with progress tracking. Existing bytes
        // are part of the logical completed length when resuming.
        self.completed_bytes = write_offset;
        {
            let g = self.group.recover();
            g.update_progress(write_offset);
        }
        let mut buffer = vec![0u8; constants::FTP_BUFFER_SIZE];
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        info!("Starting data reception from FTP server");

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
                        "FTP download halted"
                    }
                };
                drop(data_stream);
                let _ = writer.finalize().await;
                self.flush_checkpoint().await;
                ctrl.abort_transfer().await;
                ctrl.quit().await.ok();
                return Err(FtpAttemptError::from(Aria2Error::DownloadFailed(
                    halt_error.into(),
                )));
            }

            let lifecycle_notify = self.group.recover().lifecycle_notifier();
            let lifecycle_changed = lifecycle_notify.notified();
            tokio::pin!(lifecycle_changed);
            lifecycle_changed.as_mut().enable();
            let bytes_read = tokio::select! {
                bytes_read = data_stream.read(&mut buffer) => match bytes_read {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    use std::io::ErrorKind;
                    let error = match error.kind() {
                        ErrorKind::Interrupted
                        | ErrorKind::WouldBlock
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                        | ErrorKind::TimedOut => {
                            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                                message: format!("Data read error (transient): {}", error),
                            })
                        }
                        _ => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            message: format!("Data read error: {}", error),
                        }),
                    };
                    drop(data_stream);
                    self.finalize_partial_writer(&mut writer).await;
                    ctrl.abort_transfer().await;
                    ctrl.quit().await.ok();
                    return Err(FtpAttemptError::from(error));
                }
                },
                _ = &mut lifecycle_changed => {
                    let halted = {
                        let group = self.group.recover();
                        group.is_removed()
                            || group.is_force_halt_requested()
                            || group.is_halt_requested()
                    };
                    if !halted {
                        // Save-session and other non-terminal lifecycle
                        // notifications do not cancel an in-flight transfer.
                        continue;
                    }

                    let halt_error = {
                        let group = self.group.recover();
                        if group.is_removed() {
                            "Download cancelled by user"
                        } else if group.is_paused_flag() {
                            "Download paused"
                        } else {
                            "FTP download halted"
                        }
                    };
                    drop(data_stream);
                    self.finalize_partial_writer(&mut writer).await;
                    self.flush_checkpoint().await;
                    // The data connection may still be in a server-side
                    // transfer. Dropping the control connection is the only
                    // bounded cleanup here; waiting for ABOR/QUIT can block
                    // behind the stalled data transfer.
                    return Err(FtpAttemptError::from(Aria2Error::DownloadFailed(
                        halt_error.into(),
                    )));
                }
            };

            if bytes_read == 0 {
                debug!("End of data stream reached");
                break;
            }

            self.group.recover().record_network_activity();

            // Write to disk (with rate limiting if enabled)
            if let Err(error) = writer.write(&buffer[..bytes_read]).await {
                drop(data_stream);
                self.finalize_partial_writer(&mut writer).await;
                ctrl.abort_transfer().await;
                ctrl.quit().await.ok();
                return Err(FtpAttemptError::from(error));
            }
            self.completed_bytes += bytes_read as u64;
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                let save_requested = self.group.recover().take_save_control_file_request();
                checkpoint
                    .update(self.completed_bytes, save_requested)
                    .await;
            }

            // Update progress in request group
            {
                let g = self.group.recover();
                g.update_progress(self.completed_bytes);

                // Update speed calculation every 500ms
                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= constants::FTP_SPEED_UPDATE_INTERVAL_MS as u128 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = if elapsed.as_secs_f64() > 0.0 {
                        (delta as f64 / elapsed.as_secs_f64()) as u64
                    } else {
                        0
                    };
                    g.update_speed(speed, 0);
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        if let Some(expected_size) = file_size
            && self.completed_bytes != expected_size
        {
            drop(data_stream);
            self.finalize_partial_writer(&mut writer).await;
            ctrl.abort_transfer().await;
            ctrl.quit().await.ok();
            return Err(FtpAttemptError::from(Aria2Error::FtpProtocol(format!(
                "FTP transfer length mismatch: expected {}, got {}",
                expected_size, self.completed_bytes
            ))));
        }

        // Step 13: Cleanup and finalize
        drop(data_stream); // Close data connection

        // Finalize disk writer (flush buffers, etc.)
        let mut finalized_data = match writer.finalize().await {
            Ok(data) => data,
            Err(error) => {
                self.flush_checkpoint().await;
                ctrl.abort_transfer().await;
                ctrl.quit().await.ok();
                return Err(FtpAttemptError::from(Aria2Error::Fatal(
                    FatalError::Config(format!("Finalize writer failed: {}", error)),
                )));
            }
        };
        drop(writer);

        let checksum_config = self.group.recover().options().checksum.clone();
        if let Some((algorithm, expected)) = checksum_config {
            let hash_type = crate::checksum::message_digest::HashType::from_str(&algorithm)
                .ok_or_else(|| {
                    Aria2Error::Parse(format!("unknown checksum algorithm: {}", algorithm))
                })?;
            let checksum = Checksum::new(hash_type, &expected)?;
            let verified = if in_memory_download {
                let (data, verified) = checksum.verify_async(finalized_data).await?;
                finalized_data = data;
                verified
            } else {
                crate::checksum::check_integrity::man::enqueue_file_checksum_for_group(
                    &crate::checksum::check_integrity::man::shared(),
                    std::sync::Arc::clone(&self.group),
                    &self.output_path,
                    self.completed_bytes,
                    checksum,
                )
                .await?
            };
            if !verified {
                return Err(FtpAttemptError::from(Aria2Error::Checksum(format!(
                    "{} checksum mismatch for {}",
                    algorithm,
                    self.output_path.display()
                ))));
            }
            self.group.recover().set_checksum_verified(true);
        }

        self.apply_remote_time(remote_modified_time, in_memory_download);

        if in_memory_download {
            let group = self.group.recover();
            group.set_total_length(self.completed_bytes);
            group.set_completed_length(self.completed_bytes);
            group.set_in_memory_data(finalized_data);
        }

        // Read transfer completion response from control channel
        ctrl.read_transfer_complete().await?;

        self.complete_checkpoint().await;

        // Disconnect gracefully
        ctrl.quit().await.ok();

        // Calculate final statistics
        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
            }
        };

        // Update final status in request group
        {
            let g = self.group.recover();
            g.update_progress(self.completed_bytes);
            g.update_speed(final_speed, 0);
            drop(g);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        Ok(())
    }
}
