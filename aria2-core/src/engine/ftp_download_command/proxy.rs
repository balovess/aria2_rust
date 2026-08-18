//! FTP-over-HTTP proxy execution.
//!
//! The forward-proxy path is deliberately separate from the FTP control
//! command. In aria2's default `proxy-method=get` mode the proxy owns the FTP
//! session and returns an HTTP payload, so opening a second FTP control
//! connection would change the wire contract and waste a round trip.

use std::time::Instant;

use tracing::{debug, info};
use url::Url;

use crate::checksum::checksum::Checksum;
use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError};
use crate::filesystem::disk_writer::{DiskWriter, new_sequential_download_writer};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;

use super::execution::FtpAttemptError;
use super::types::FtpDownloadCommand;

impl FtpDownloadCommand {
    pub(super) async fn execute_proxy_get_attempt(
        &mut self,
        proxy: &crate::ftp::connection::FtpProxyConfig,
    ) -> std::result::Result<(), FtpAttemptError> {
        let (uri, options, in_memory_download) = {
            let group = self.group.recover();
            let uri = group.uris().first().cloned().ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config(
                    "FTP proxy download has no source URI".to_string(),
                ))
            })?;
            (uri, group.options_arc(), group.is_in_memory_download())
        };
        let ftp_url = Url::parse(&uri).map_err(|error| {
            FtpAttemptError::from(Aria2Error::Fatal(FatalError::Config(format!(
                "Invalid FTP URI '{}': {}",
                uri, error
            ))))
        })?;

        let local_size = if in_memory_download {
            0
        } else {
            std::fs::metadata(&self.output_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        };
        let requested_offset = if options.continue_download {
            local_size
        } else {
            0
        };

        let mut response = crate::ftp::connection::execute_proxy_get(
            ftp_url,
            proxy,
            requested_offset,
            options.http_no_cache,
        )
        .await?;
        let status = response.head.status_code;
        if !(200..300).contains(&status) {
            return Err(FtpAttemptError::from(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!(
                        "FTP proxy GET failed: {} {}",
                        status, response.head.reason_phrase
                    ),
                },
            )));
        }

        let resumed = requested_offset > 0 && status == 206;
        let write_offset = if resumed { requested_offset } else { 0 };
        if requested_offset > 0 && !resumed && !in_memory_download {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&self.output_path)
                .and_then(|file| file.set_len(0))
                .map_err(|error| {
                    FtpAttemptError::from(Aria2Error::FileIo(format!(
                        "truncate FTP proxy target {}: {}",
                        self.output_path.display(),
                        error
                    )))
                })?;
        }

        let body_length = response.head.content_length();
        let total_length = body_length.map(|length| length.saturating_add(write_offset));
        let remote_modified_time = options
            .remote_time
            .then(|| response.head.header("last-modified"))
            .flatten()
            .and_then(crate::http::cookie::parsing::parse_http_date)
            .and_then(|seconds| {
                (seconds >= 0).then_some(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64),
                )
            });
        if options.dry_run {
            self.completed_bytes = total_length.unwrap_or_default();
            {
                let group = self.group.recover();
                if let Some(total_length) = total_length {
                    group.set_total_length(total_length);
                }
                group.update_progress(self.completed_bytes);
                group.set_checksum_verified(true);
            }
            self.group.recover_mut().complete()?;
            return Ok(());
        }

        if let Some(total_length) = total_length {
            let checkpoint_offset =
                crate::engine::progress_checkpoint::ProgressCheckpoint::resume_input_length(
                    &self.output_path,
                    local_size,
                    options.continue_download,
                    total_length,
                )
                .await;
            {
                let group = self.group.recover();
                group.set_total_length(total_length);
            }
            if !in_memory_download {
                self.checkpoint = Some(
                    crate::engine::progress_checkpoint::ProgressCheckpoint::open(
                        &self.output_path,
                        total_length,
                        checkpoint_offset,
                    )
                    .await,
                );
            }
        }

        let raw_writer = new_sequential_download_writer(
            &self.output_path,
            in_memory_download,
            write_offset,
            total_length,
        );
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|limiter| limiter.is_download_limited());
        let mut writer: Box<dyn DiskWriter> =
            if options.max_download_limit.is_some() || global_limited {
                let limiter = options
                    .max_download_limit
                    .filter(|&limit| limit > 0)
                    .map(|limit| RateLimiter::new(&RateLimiterConfig::new(Some(limit), None)))
                    .unwrap_or_else(RateLimiter::unlimited);
                let mut throttled = ThrottledWriter::new(raw_writer, limiter);
                if let Some(global_limiter) = &self.global_limiter {
                    throttled = throttled.with_global_limiter(global_limiter.clone());
                }
                Box::new(throttled)
            } else {
                raw_writer
            };

        self.completed_bytes = write_offset;
        {
            let group = self.group.recover();
            group.update_progress(write_offset);
        }

        let mut body_bytes = 0u64;
        let mut buffer = vec![0u8; constants::FTP_BUFFER_SIZE];
        let start_time = Instant::now();
        loop {
            let halted = {
                let group = self.group.recover();
                group.is_removed() || group.is_force_halt_requested() || group.is_halt_requested()
            };
            if halted {
                self.finalize_partial_writer(&mut writer).await;
                return Err(FtpAttemptError::from(Aria2Error::DownloadFailed(
                    "FTP proxy download halted".into(),
                )));
            }

            let bytes_read = match response.read_body(&mut buffer).await {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    self.finalize_partial_writer(&mut writer).await;
                    return Err(FtpAttemptError::from(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!("FTP proxy body read failed: {}", error),
                        },
                    )));
                }
            };
            if bytes_read == 0 {
                break;
            }
            // Refresh the inactivity clock when bytes arrive from the proxy,
            // before any disk write or rate limiting can delay the loop.
            self.group.recover().record_network_activity();
            if let Err(error) = writer.write(&buffer[..bytes_read]).await {
                self.finalize_partial_writer(&mut writer).await;
                return Err(FtpAttemptError::from(error));
            }
            body_bytes = body_bytes.saturating_add(bytes_read as u64);
            self.completed_bytes = self.completed_bytes.saturating_add(bytes_read as u64);
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                let save_requested = self.group.recover().take_save_control_file_request();
                checkpoint
                    .update(self.completed_bytes, save_requested)
                    .await;
            }
            self.group.recover().update_progress(self.completed_bytes);
        }

        if let Some(expected_length) = body_length
            && body_bytes != expected_length
        {
            self.finalize_partial_writer(&mut writer).await;
            return Err(FtpAttemptError::from(Aria2Error::FtpProtocol(format!(
                "FTP proxy transfer length mismatch: expected {}, got {}",
                expected_length, body_bytes
            ))));
        }

        let finalized_data = match writer.finalize().await {
            Ok(data) => data,
            Err(error) => {
                self.flush_checkpoint().await;
                return Err(FtpAttemptError::from(error));
            }
        };
        drop(writer);

        if let Some((algorithm, expected)) = options.checksum.clone() {
            let hash_type = crate::checksum::message_digest::HashType::from_str(&algorithm)
                .ok_or_else(|| {
                    Aria2Error::Parse(format!("unknown checksum algorithm: {}", algorithm))
                })?;
            let checksum = Checksum::new(hash_type, &expected)?;
            let verified = if in_memory_download {
                checksum.verify(&finalized_data)
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

        self.complete_checkpoint().await;
        let final_speed = start_time.elapsed().as_secs_f64().max(f64::EPSILON);
        {
            let group = self.group.recover();
            group.update_speed((self.completed_bytes as f64 / final_speed) as u64, 0);
            drop(group);
            self.group.recover_mut().complete()?;
        }
        info!(
            path = %self.output_path.display(),
            bytes = self.completed_bytes,
            "FTP proxy GET download completed"
        );
        debug!(proxy = %proxy.proxy_host, "FTP proxy GET connection closed");
        Ok(())
    }
}
