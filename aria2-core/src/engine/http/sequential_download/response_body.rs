use futures::StreamExt;

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;

use super::SequentialDownloader;
use super::download_flow::finalize_cancelled_download;

impl SequentialDownloader {
    /// Download the response body to the output file.
    ///
    /// Extracted from `execute()` so it can be reused by the auth retry path.
    /// Assumes the response status is 2xx or 206.
    pub(in crate::engine::http::sequential_download) async fn download_response_body(
        &mut self,
        response: reqwest::Response,
        _uri: &str,
        resume_state: &ResumeState,
    ) -> Result<()> {
        let resp_length = response.content_length().unwrap_or(0);
        let actual_total = if resume_state.should_resume {
            resume_state.start_offset + resp_length
        } else {
            resp_length
        };
        {
            let g = self.group.recover();
            g.set_total_length(actual_total);
        }

        // Extract Last-Modified header for remote-time option.
        // C++ `updateLastModifiedTime()`: when the `remote-time` option is
        // enabled, the file's mtime is set to the server's Last-Modified time
        // after download completion.
        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let start_offset = if resume_state.should_resume {
            resume_state.start_offset
        } else {
            0
        };

        self.progress_updater.reset(start_offset);

        let rate_limit = { self.group.recover().options().max_download_limit };

        let raw_writer = if start_offset > 0 {
            DefaultDiskWriter::new_with_offset(&self.output_path, start_offset)
        } else {
            DefaultDiskWriter::new(&self.output_path)
        };

        // Build per-download limiter (if max_download_limit is set).
        let per_limiter = match rate_limit {
            Some(rate) if rate > 0 => {
                let cfg = RateLimiterConfig::new(Some(rate), None);
                let limiter = RateLimiter::new(&cfg);
                tracing::debug!("Download speed limit enabled: {} bytes/s", rate);
                {
                    let g = self.group.recover();
                    g.set_rate_limiter(limiter.clone());
                }
                Some(limiter)
            }
            _ => None,
        };

        // Create ThrottledWriter when either per-download or global limit is active.
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());
        let mut writer: Box<dyn DiskWriter> = if per_limiter.is_some() || global_limited {
            let limiter = per_limiter.unwrap_or_else(RateLimiter::unlimited);
            let mut tw = ThrottledWriter::new(raw_writer, limiter);
            if let Some(ref gl) = self.global_limiter {
                tw = tw.with_global_limiter(gl.clone());
            }
            Box::new(tw)
        } else {
            Box::new(raw_writer)
        };

        let mut stream = response.bytes_stream();
        let mut completed_bytes = start_offset;
        let write_piece = constants::RATE_LIMITER_CHUNK_SIZE;

        // ADR-0001: Create control file for sequential downloads too.
        // Even without piece-level tracking, the control file's
        // completed_length is the authoritative source for resume detection,
        // immune to the preallocation pitfall.
        let ctrl_path = ControlFile::control_path_for(&self.output_path);
        let mut ctrl_file = if actual_total > 0 {
            match ControlFile::open_or_create(&ctrl_path, actual_total, 1).await {
                Ok(mut cf) => {
                    if start_offset > 0 {
                        cf.update_completed_length(start_offset);
                    }
                    if let Err(e) = cf.save().await {
                        tracing::warn!("Sequential: control file save failed: {}", e);
                    }
                    Some(cf)
                }
                Err(e) => {
                    tracing::warn!(
                        "Sequential: control file creation failed {}: {}",
                        ctrl_path.display(),
                        e
                    );
                    None
                }
            }
        } else {
            None
        };
        let mut ctrl_bytes_since_save: u64 = 0;
        let ctrl_save_interval = (actual_total / 10).max(1024 * 1024); // save every ~10% or 1MB

        let lifecycle_notifier = self.group.recover().lifecycle_notifier();
        loop {
            let next_chunk = {
                let lifecycle_changed = lifecycle_notifier.notified();
                tokio::pin!(lifecycle_changed);
                lifecycle_changed.as_mut().enable();
                tokio::select! {
                    chunk = stream.next() => chunk,
                    _ = &mut lifecycle_changed => {
                        if let Err(error) = self.check_cancelled() {
                            finalize_cancelled_download(
                                &mut writer,
                                &mut ctrl_file,
                                completed_bytes,
                            )
                            .await;
                            return Err(error);
                        }
                        self.flush_requested_control_file(
                            &mut writer,
                            &mut ctrl_file,
                            completed_bytes,
                        )
                        .await?;
                        continue;
                    }
                }
            };
            let Some(chunk) = next_chunk else { break };

            // Check whether the task was removed between chunks. This is the
            // primary cancellation signal: `aria2.remove` /
            // `aria2.forceRemove` sets the RequestGroup status to `Removed`,
            // which `is_removed()` observes without blocking.
            if let Err(e) = self.check_cancelled() {
                finalize_cancelled_download(&mut writer, &mut ctrl_file, completed_bytes).await;
                return Err(e);
            }

            let data: bytes::Bytes = chunk.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;
            if !data.is_empty() {
                self.progress.record_network_activity();
            }

            let mut offset = 0usize;
            while offset < data.len() {
                let end = (offset + write_piece).min(data.len());
                let piece = &data[offset..end];
                writer.write(piece).await?;
                completed_bytes += piece.len() as u64;
                offset = end;

                // ADR-0001: Periodically update control file with progress.
                ctrl_bytes_since_save += piece.len() as u64;
                let save_requested = self
                    .flush_requested_control_file(&mut writer, &mut ctrl_file, completed_bytes)
                    .await?;
                if let Some(cf) = ctrl_file.as_mut()
                    && (save_requested || ctrl_bytes_since_save >= ctrl_save_interval)
                {
                    cf.update_completed_length(completed_bytes);
                    if let Err(e) = cf.save().await {
                        tracing::warn!("Sequential: control file save failed: {}", e);
                    }
                    ctrl_bytes_since_save = 0;
                }

                self.progress_updater
                    .update_progress(
                        completed_bytes,
                        constants::PROGRESS_UPDATE_BYTES as u64,
                        constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                    )
                    .await;
            }
        }

        writer.finalize().await.map_err(|error| {
            Aria2Error::FileIo(format!("Failed to finalize downloaded file: {error}"))
        })?;

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
            self.progress.set_completed_length(completed_bytes);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(0);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        tracing::info!(
            "Sequential download complete: {} ({} bytes)",
            self.output_path.display(),
            completed_bytes
        );

        // Apply remote-time: set file mtime to server's Last-Modified.
        // Matches C++ `updateLastModifiedTime()`:
        //   if (getOption()->getAsBool(PREF_REMOTE_TIME)) {
        //     getRequestGroup()->updateLastModifiedTime(lastModified);
        //   }
        // The actual file mtime update happens here, after the file is closed.
        if let Some(ref lm_str) = last_modified {
            let g = self.group.recover();
            if g.options().remote_time {
                // Use the cookie module's RFC 6265 HTTP-date parser which
                // supports IMF-fixdate, RFC 850, and asctime formats.
                if let Some(epoch_secs) = crate::http::cookie::parsing::parse_http_date(lm_str) {
                    let mtime_file =
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs as u64);
                    // Use std::fs::metadata + set_file_mtime via filetime crate
                    // for cross-platform support. If filetime is not available,
                    // we can use platform-specific calls.
                    // For now, use std::fs which supports setting modification time.
                    if let Err(e) = std::fs::File::open(&self.output_path).and_then(|f| {
                        f.set_modified(mtime_file)
                            .map_err(|e2| std::io::Error::new(e2.kind(), e2.to_string()))
                    }) {
                        tracing::warn!(
                            "Failed to set file mtime from Last-Modified '{}': {}",
                            lm_str,
                            e
                        );
                    } else {
                        tracing::debug!("Set file mtime from Last-Modified: {}", lm_str);
                    }
                }
            }
        }

        // ADR-0001: Delete control file on successful completion.
        drop(ctrl_file);
        if ctrl_path.exists()
            && let Err(e) = tokio::fs::remove_file(&ctrl_path).await
        {
            tracing::debug!("Failed to delete control file on completion: {}", e);
        }
        self.cookie_helper.save_cookies_if_configured();
        Ok(())
    }
}
