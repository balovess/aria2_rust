// Gap-based sequential download: download missing byte ranges
// for partially-completed files.

use futures::StreamExt;

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::http::response::is_redirect_status;
use crate::http::response_processor::range::parse_content_range_value;
use crate::http::skip_response::MAX_REDIRECT_COUNT;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group::ActiveConnectionGuard;
use crate::util::rwlock_ext::RwLockRecover;

use super::{GapDownloadResult, SequentialDownloader};

fn classify_gap_http_status(status_code: u16, range_header: &str) -> Aria2Error {
    match status_code {
        200 => Aria2Error::Recoverable(RecoverableError::CannotResume),
        416 => Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable {
            range: range_header.to_string(),
        }),
        code if code >= 500 || constants::RETRYABLE_HTTP_CODES.contains(&code) => {
            Aria2Error::Recoverable(RecoverableError::ServerError { code })
        }
        _ => Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
            message: format!("HTTP error: {status_code}"),
        }),
    }
}

impl SequentialDownloader {
    /// Download only the missing byte ranges (gaps) for a partially-completed
    /// file. Each gap is fetched with a separate Range request.
    pub async fn execute_with_gaps(
        &mut self,
        uri: &str,
        total_length: u64,
        completed_ranges: &[(u64, u64)],
    ) -> GapDownloadResult {
        let connection_guard = ActiveConnectionGuard::new(std::sync::Arc::clone(&self.group));
        connection_guard.set(1);
        let gaps = Self::find_all_gaps(completed_ranges, total_length);
        tracing::info!(
            "Starting sequential download with gaps: uri={}, total={}, gaps={:?}",
            uri,
            total_length,
            gaps
        );

        if gaps.is_empty() {
            tracing::info!("No gaps to download, download complete");
            return GapDownloadResult {
                completed_gaps: Vec::new(),
                error: None,
            };
        }

        let missing_bytes = gaps
            .iter()
            .fold(0u64, |total, (_, length)| total.saturating_add(*length));
        let mut completed_bytes = total_length.saturating_sub(missing_bytes);
        self.progress_updater.reset(completed_bytes);

        let disk_cache = self.group.recover().options().disk_cache_size_bytes();
        let mut writer = CachedDiskWriter::new_with_mmap_bytes(
            &self.output_path,
            Some(total_length),
            disk_cache,
            false,
        );
        // The split attempt this falls back from left its checkpoint. Record
        // filled gaps in it, so a retry or the next run keeps them.
        let ctrl_path = ControlFile::control_path_for(&self.output_path);
        let mut checkpoint = GapCheckpoint {
            control_file: ControlFile::load(&ctrl_path)
                .await
                .ok()
                .flatten()
                .filter(|cf| cf.total_length() == total_length && !cf.is_torrent_checkpoint()),
            on_disk: Self::merge_ranges(completed_ranges),
        };

        let rate_limit = { self.group.recover().options().max_download_limit };
        let limiter = rate_limit
            .filter(|&r| r > 0)
            .map(|r| RateLimiter::new(&RateLimiterConfig::new(Some(r), None)));
        if let Some(ref lim) = limiter {
            let g = self.group.recover();
            g.set_rate_limiter(lim.clone());
        }

        let mut last_progress_update = completed_bytes;
        let mut completed_gaps: Vec<(u64, u64)> = Vec::new();

        for (gap_start, gap_length) in gaps {
            // Check whether the task was removed before starting the next gap.
            // This is the primary cancellation signal: `aria2.remove` /
            // `aria2.forceRemove` sets the RequestGroup status to `Removed`,
            // which `is_removed()` observes without blocking.
            if let Err(e) = self.check_cancelled() {
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(e),
                };
            }

            let gap_end = gap_start + gap_length - 1;
            let range_header = format!("bytes={}-{}", gap_start, gap_end);
            tracing::debug!("Sequential Range request for gap: {}", range_header);

            let response = match self.send_gap_request(uri, &range_header).await {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(
                        "Gap download failed ({}), cleaning up partial data",
                        range_header
                    );
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(error),
                    };
                }
            };

            let is_http2 = response.version() == reqwest::Version::HTTP_2;
            connection_guard.set_http_counts(
                usize::from(!is_http2),
                usize::from(is_http2),
                usize::from(is_http2),
            );

            let status = response.status();
            if status.as_u16() != 206 {
                tracing::warn!(
                    "Gap download failed with HTTP status {} ({}), cleaning up partial data",
                    status,
                    range_header
                );
                let error = if status.as_u16() == 404 {
                    self.classify_file_not_found()
                } else {
                    classify_gap_http_status(status.as_u16(), &range_header)
                };
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(error),
                };
            }

            let content_range = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range_value);
            if content_range != Some((gap_start, gap_end, total_length)) {
                tracing::warn!(
                    "Gap download returned an unexpected Content-Range ({:?}) for {}",
                    content_range,
                    range_header
                );
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(Aria2Error::Recoverable(RecoverableError::CannotResume)),
                };
            }

            let mut stream = response.bytes_stream();
            let mut stream_offset = gap_start;
            let mut bytes_downloaded = 0u64;

            while let Some(chunk_result) = tokio::select! {
                chunk_result = stream.next() => chunk_result,
                cancellation = self.wait_for_cancellation() => {
                    let error = cancellation.expect_err(
                        "cancellation watcher must not complete successfully",
                    );
                    Self::cleanup_partial_gap(
                        &mut writer,
                        &mut checkpoint,
                        gap_start,
                        bytes_downloaded,
                    )
                    .await;
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(error),
                    };
                }
            } {
                // Check whether the task was removed between chunks. This is
                // the primary cancellation signal: `aria2.remove` /
                // `aria2.forceRemove` sets the RequestGroup status to
                // `Removed`, which `is_removed()` observes without blocking.
                if let Err(e) = self.check_cancelled() {
                    Self::cleanup_partial_gap(
                        &mut writer,
                        &mut checkpoint,
                        gap_start,
                        bytes_downloaded,
                    )
                    .await;
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(e),
                    };
                }

                let data: bytes::Bytes = match chunk_result {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(
                            "Gap download failed during streaming ({}), cleaning up partial data",
                            range_header
                        );
                        Self::cleanup_partial_gap(
                            &mut writer,
                            &mut checkpoint,
                            gap_start,
                            bytes_downloaded,
                        )
                        .await;
                        return GapDownloadResult {
                            completed_gaps,
                            error: Some(Aria2Error::Recoverable(
                                RecoverableError::TemporaryNetworkFailure {
                                    message: e.to_string(),
                                },
                            )),
                        };
                    }
                };

                if !data.is_empty() {
                    self.progress.record_network_activity();
                }

                let data_len = data.len() as u64;
                if bytes_downloaded.saturating_add(data_len) > gap_length {
                    tracing::warn!("Gap download exceeded expected size for {}", range_header);
                    Self::cleanup_partial_gap(
                        &mut writer,
                        &mut checkpoint,
                        gap_start,
                        bytes_downloaded,
                    )
                    .await;
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(Aria2Error::Recoverable(
                            RecoverableError::HttpProtocolError {
                                message: format!(
                                    "HTTP Range response exceeded expected length for {range_header}"
                                ),
                            },
                        )),
                    };
                }

                if let Some(ref lim) = limiter {
                    lim.acquire_download(data_len).await;
                }
                if let Some(ref gl) = self.global_limiter
                    && gl.is_download_limited()
                {
                    gl.acquire_download(data_len).await;
                }

                if let Err(error) = writer.write_bytes_at(stream_offset, data).await {
                    tracing::warn!(
                        "Write failed during gap download ({}), cleaning up partial data",
                        range_header
                    );
                    Self::cleanup_partial_gap(
                        &mut writer,
                        &mut checkpoint,
                        gap_start,
                        bytes_downloaded,
                    )
                    .await;
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(Aria2Error::Fatal(crate::error::FatalError::Config(
                            format!("Write failed: {}", error),
                        ))),
                    };
                }

                completed_bytes += data_len;
                stream_offset += data_len;
                bytes_downloaded += data_len;

                if completed_bytes - last_progress_update >= constants::PROGRESS_UPDATE_BYTES as u64
                {
                    self.progress_updater
                        .update_progress(
                            completed_bytes,
                            constants::PROGRESS_UPDATE_BYTES as u64,
                            constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                        )
                        .await;
                    last_progress_update = completed_bytes;
                }
            }

            if bytes_downloaded == gap_length {
                // A later gap can still fail this attempt, and the retry
                // skips this one: its bytes must not stay in the write cache.
                if let Err(error) = checkpoint.persist(&mut writer, gap_start, gap_length).await {
                    tracing::warn!("Flush failed during gap download: {}", error);
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(Aria2Error::Fatal(crate::error::FatalError::Config(
                            format!("Flush failed: {}", error),
                        ))),
                    };
                }
                completed_gaps.push((gap_start, gap_length));
                tracing::debug!(
                    "Gap download complete: {} ({} bytes)",
                    range_header,
                    bytes_downloaded
                );
            } else {
                tracing::warn!(
                    "Gap download incomplete: expected {} bytes, got {} bytes",
                    gap_length,
                    bytes_downloaded
                );
                Self::cleanup_partial_gap(
                    &mut writer,
                    &mut checkpoint,
                    gap_start,
                    bytes_downloaded,
                )
                .await;
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!(
                                "HTTP Range response ended early: expected {gap_length} bytes, got {bytes_downloaded}"
                            ),
                        },
                    )),
                };
            }
        }

        if let Err(e) = writer.sync_all().await {
            tracing::warn!("Flush failed during gap download: {}", e);
            return GapDownloadResult {
                completed_gaps,
                error: Some(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("Flush failed: {}", e),
                ))),
            };
        }

        let final_speed = {
            let g = self.group.recover();
            let elapsed = g.elapsed_time();
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (completed_bytes as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };
        {
            self.progress.set_total_length(completed_bytes);
            self.progress.set_completed_length(completed_bytes);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(0);
            let mut g = self.group.recover_mut();
            if let Err(e) = g.complete() {
                tracing::warn!("Failed to complete request group: {}", e);
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(e),
                };
            }
        }

        tracing::info!(
            "Sequential download with gaps complete: {} ({} bytes, {} gaps completed)",
            self.output_path.display(),
            completed_bytes,
            completed_gaps.len()
        );
        drop(checkpoint);
        if let Err(error) = crate::filesystem::durability::remove_file(&ctrl_path).await {
            tracing::debug!("Failed to delete control file on completion: {}", error);
        }
        self.cookie_helper.save_cookies_if_configured();
        GapDownloadResult {
            completed_gaps,
            error: None,
        }
    }

    async fn send_gap_request(&self, uri: &str, range_header: &str) -> Result<reqwest::Response> {
        let mut current_url = reqwest::Url::parse(uri).map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("invalid HTTP Range URL {uri}: {error}"),
            })
        })?;
        let mut redirect_count = 0;

        loop {
            self.request_policy.wait_before_request().await;
            let cookie_header = self
                .cookie_helper
                .build_cookie_header_from_url(&current_url);
            let cookie_header = (!cookie_header.is_empty()).then_some(cookie_header);
            let request = self.request_policy.apply(
                self.client
                    .get(current_url.as_str())
                    .header(reqwest::header::RANGE, range_header),
                cookie_header.as_deref(),
                &[],
            );
            let response = tokio::select! {
                result = request.send() => result.map_err(|error| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("HTTP Range request failed: {error}"),
                    })
                })?,
                cancellation = self.wait_for_cancellation() => {
                    return Err(cancellation.expect_err(
                        "cancellation watcher must not complete successfully",
                    ));
                }
            };

            let status_code = response.status().as_u16();
            self.publish_connection_context(current_url.as_str(), response.remote_addr());
            self.cookie_helper
                .extract_and_store_cookies(current_url.as_str(), &response);

            if !is_redirect_status(status_code) {
                return Ok(response);
            }

            if redirect_count >= MAX_REDIRECT_COUNT {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::HttpTooManyRedirects {
                        count: redirect_count,
                    },
                ));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
                        message: format!("HTTP {status_code} redirect without Location header"),
                    })
                })?;
            let target_url = current_url.join(location).map_err(|error| {
                Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
                    message: format!("failed to resolve redirect URL '{location}': {error}"),
                })
            })?;
            tracing::info!(
                status_code,
                redirect_count = redirect_count + 1,
                from = %current_url,
                to = %target_url,
                "HTTP Range redirect while filling a gap"
            );
            self.group
                .recover_mut()
                .add_redirect_uri(target_url.as_str());
            current_url = target_url;
            redirect_count += 1;
        }
    }

    /// Keep a partially-written gap in the checkpoint, or without one
    /// zero-fill it to maintain data integrity.
    async fn cleanup_partial_gap(
        writer: &mut CachedDiskWriter,
        checkpoint: &mut GapCheckpoint,
        gap_start: u64,
        bytes_written: u64,
    ) {
        if bytes_written == 0 {
            return;
        }
        if checkpoint.control_file.is_some() {
            if let Err(error) = checkpoint.persist(writer, gap_start, bytes_written).await {
                tracing::warn!(
                    "Failed to checkpoint partial gap at {} ({} bytes): {}",
                    gap_start,
                    bytes_written,
                    error
                );
            }
            return;
        }
        let zero_data = vec![0u8; bytes_written as usize];
        if let Err(error) = writer.write_at(gap_start, &zero_data).await {
            tracing::warn!(
                "Failed to cleanup partial gap at {} ({} bytes): {}",
                gap_start,
                bytes_written,
                error
            );
        }
    }
}

/// The ranges of a gap download that are on disk, and the checkpoint that
/// records them.
struct GapCheckpoint {
    control_file: Option<ControlFile>,
    /// Merged `(offset, length)` ranges known to be written.
    on_disk: Vec<(u64, u64)>,
}

impl GapCheckpoint {
    /// Make `[start, start + length)` durable and record it. The checkpoint
    /// gets the whole written run that ends there, so a piece whose in-flight
    /// prefix ends mid-block is still finished by the gap after it.
    async fn persist(
        &mut self,
        writer: &mut CachedDiskWriter,
        start: u64,
        length: u64,
    ) -> Result<()> {
        // A completed range must cross the positioned writer's stable-storage
        // and namespace barrier before it is recorded in the checkpoint.
        writer.sync_all().await?;
        let Some(control_file) = self.control_file.as_mut() else {
            return Ok(());
        };
        let run_start = self
            .on_disk
            .iter()
            .find(|&&(offset, len)| offset <= start && start <= offset + len)
            .map_or(start, |&(offset, _)| offset);
        control_file.record_completed_range(run_start, start + length - run_start);
        if let Err(error) = control_file.save().await {
            tracing::warn!("Control file save after gap failed: {}", error);
        }
        self.on_disk.push((start, length));
        self.on_disk = SequentialDownloader::merge_ranges(&self.on_disk);
        Ok(())
    }
}

#[cfg(test)]
#[path = "gap_download/tests.rs"]
mod tests;
