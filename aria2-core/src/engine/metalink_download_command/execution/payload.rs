//! HTTP payload streaming and resumable output handling for Metalink.

use futures::StreamExt;
use std::path::Path;
use std::time::Instant;

use super::{MetalinkDownloadCommand, PayloadDownload, classify_metalink_http_status};
use crate::engine::progress_checkpoint::ProgressCheckpoint;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;

impl MetalinkDownloadCommand {
    pub(super) async fn download_payload_url(
        &mut self,
        output_path: &Path,
        url: &str,
        expected_size: Option<u64>,
    ) -> Result<PayloadDownload> {
        let existing_length = tokio::fs::metadata(output_path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let total_length = expected_size
            .or(ProgressCheckpoint::stored_total_length(output_path).await)
            .unwrap_or(0);
        let continue_download = self.group.recover().options().continue_download;
        let existing_length = if total_length > 0 && existing_length > total_length {
            truncate_output(output_path).await?;
            0
        } else {
            existing_length
        };
        let resume_input_length = if total_length > 0 {
            ProgressCheckpoint::resume_input_length(
                output_path,
                existing_length,
                continue_download,
                total_length,
            )
            .await
        } else if continue_download {
            existing_length
        } else {
            0
        };

        self.checkpoint = if total_length > 0 {
            Some(ProgressCheckpoint::open(output_path, total_length, resume_input_length).await)
        } else {
            None
        };
        let resume_offset = self
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.resume_offset(resume_input_length))
            .unwrap_or(resume_input_length);

        if let Some(lifecycle_error) = self.lifecycle_error() {
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(resume_offset, true).await;
            }
            self.completed_bytes = resume_offset;
            return Err(lifecycle_error);
        }

        let mut request = self.client.get(url);
        if resume_offset > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={resume_offset}-"));
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                if let Some(checkpoint) = self.checkpoint.as_mut() {
                    checkpoint.update(resume_offset, true).await;
                }
                self.completed_bytes = resume_offset;
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("HTTP request failed: {error}"),
                    },
                ));
            }
        };

        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(resume_offset, true).await;
            }
            return Err(classify_metalink_http_status(status.as_u16()));
        }

        if resume_offset > 0 && status.as_u16() == 200 {
            let always_resume = self.group.recover().options().always_resume;
            if !always_resume {
                truncate_output(output_path).await?;
                self.checkpoint = if total_length > 0 {
                    Some(ProgressCheckpoint::open(output_path, total_length, 0).await)
                } else {
                    None
                };
                self.completed_bytes = 0;
                return self
                    .download_payload_response(output_path, response, expected_size, 0)
                    .await;
            }
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(resume_offset, true).await;
            }
            self.completed_bytes = resume_offset;
            return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
        }

        self.download_payload_response(output_path, response, expected_size, resume_offset)
            .await
    }

    async fn download_payload_response(
        &mut self,
        output_path: &Path,
        response: reqwest::Response,
        expected_size: Option<u64>,
        resume_offset: u64,
    ) -> Result<PayloadDownload> {
        // Content-Range is authoritative for a resumed response. For a fresh
        // response, use the body length and the Metalink size when present.
        let response_length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let advertised_total = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range_total)
            .or_else(|| (response_length > 0).then_some(resume_offset + response_length))
            .or(expected_size)
            .unwrap_or(0);
        let total_length = expected_size.unwrap_or(advertised_total);

        if self.checkpoint.is_none() && total_length > 0 {
            self.checkpoint = Some(
                ProgressCheckpoint::open(
                    output_path,
                    total_length,
                    resume_offset.min(total_length),
                )
                .await,
            );
        }

        {
            let g = self.group.recover();
            g.set_total_length(total_length);
            g.update_progress(resume_offset);
        }

        let raw_writer = if resume_offset > 0 {
            DefaultDiskWriter::new_with_offset(output_path, resume_offset)
        } else {
            DefaultDiskWriter::new(output_path)
        };
        let rate_limit = self.group.recover().options().max_download_limit;
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|limiter| limiter.is_download_limited());
        let mut writer: Box<dyn DiskWriter> = if rate_limit.is_some() || global_limited {
            let per_rate = rate_limit.filter(|&rate| rate > 0);
            let limiter = per_rate
                .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)))
                .unwrap_or_else(RateLimiter::unlimited);
            let mut writer = ThrottledWriter::new(raw_writer, limiter);
            if let Some(global_limiter) = self.global_limiter.as_ref() {
                writer = writer.with_global_limiter(global_limiter.clone());
            }
            Box::new(writer)
        } else {
            Box::new(raw_writer)
        };

        self.completed_bytes = resume_offset;
        let mut stream = response.bytes_stream();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        while let Some(chunk_result) = tokio::select! {
            next = stream.next() => next,
            _ = self.wait_for_lifecycle_change() => {
                self.finalize_partial_writer(&mut writer).await;
                return Err(self.lifecycle_error().unwrap_or_else(|| {
                    Aria2Error::DownloadFailed("Metalink download halted".into())
                }));
            }
        } {
            let bytes: bytes::Bytes = match chunk_result {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.finalize_partial_writer(&mut writer).await;
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: error.to_string(),
                        },
                    ));
                }
            };
            if let Some(lifecycle_error) = self.lifecycle_error() {
                writer.finalize().await.ok();
                self.flush_checkpoint().await;
                return Err(lifecycle_error);
            }
            if !bytes.is_empty() {
                self.group.recover().record_network_activity();
            }
            if let Err(error) = writer.write(&bytes).await {
                self.finalize_partial_writer(&mut writer).await;
                return Err(Aria2Error::FileIo(format!(
                    "Failed to write Metalink payload: {error}"
                )));
            }
            self.completed_bytes = self.completed_bytes.saturating_add(bytes.len() as u64);

            if let Some(checkpoint) = self.checkpoint.as_mut() {
                let save_requested = self.group.recover().take_save_control_file_request();
                checkpoint
                    .update(self.completed_bytes, save_requested)
                    .await;
            }

            let elapsed = last_speed_update.elapsed();
            if elapsed.as_millis() >= 500 {
                let delta = self.completed_bytes - last_completed;
                let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                let g = self.group.recover();
                g.update_progress(self.completed_bytes);
                g.update_speed(speed, 0);
                last_speed_update = Instant::now();
                last_completed = self.completed_bytes;
            }
        }

        writer.finalize().await.map_err(|error| {
            Aria2Error::FileIo(format!("Failed to finalize Metalink file: {error}"))
        })?;

        if let Some(expected) = expected_size
            && self.completed_bytes != expected
        {
            self.discard_checkpoint(output_path).await;
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Metalink size mismatch: expected {} bytes, received {}",
                        expected, self.completed_bytes
                    ),
                },
            ));
        }

        Ok(PayloadDownload {
            path: output_path.to_path_buf(),
            completed_length: self.completed_bytes,
            total_length: total_length.max(self.completed_bytes),
        })
    }
}

async fn truncate_output(path: &Path) -> Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await
        .map_err(|error| Aria2Error::FileIo(error.to_string()))?;
    file.sync_data()
        .await
        .map_err(|error| Aria2Error::FileIo(error.to_string()))
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.parse().ok()
}
