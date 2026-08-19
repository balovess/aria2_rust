// Gap-based sequential download: download missing byte ranges
// for partially-completed files.

use futures::StreamExt;

use crate::constants;
use crate::error::{Aria2Error, RecoverableError};
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::util::rwlock_ext::RwLockRecover;

use super::{GapDownloadResult, SequentialDownloader};

fn classify_gap_http_status(status_code: u16, range_header: &str) -> Aria2Error {
    match status_code {
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

        let url_parsed = reqwest::Url::parse(uri).ok();
        let cookie_hdr = if let Some(ref url) = url_parsed {
            let hdr = self.cookie_helper.build_cookie_header_from_url(url);
            if hdr.is_empty() { None } else { Some(hdr) }
        } else {
            None
        };

        let mut completed_bytes = completed_ranges.iter().map(|(_, len)| len).sum::<u64>();
        self.progress_updater.reset(completed_bytes);

        let disk_cache = self.group.recover().options().disk_cache_size_bytes();
        let mut writer = CachedDiskWriter::new_with_mmap_bytes(
            &self.output_path,
            Some(total_length),
            disk_cache,
            false,
        );

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

            let request = self.request_policy.apply(
                self.client.get(uri).header("Range", &range_header),
                cookie_hdr.as_deref(),
                &[],
            );

            let response = match tokio::select! {
                result = request.send() => result,
                cancellation = self.wait_for_cancellation() => {
                    let error = cancellation.expect_err(
                        "cancellation watcher must not complete successfully",
                    );
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(error),
                    };
                }
            } {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "Gap download failed ({}), cleaning up partial data",
                        range_header
                    );
                    Self::cleanup_partial_gap(&mut writer, gap_start, 0).await;
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!("HTTP request failed: {}", e),
                            },
                        )),
                    };
                }
            };

            // Keep timeout and DNS candidate attribution consistent with the
            // normal sequential response path.
            self.publish_connection_context(uri, response.remote_addr());
            self.cookie_helper.extract_and_store_cookies(uri, &response);

            let status = response.status();
            if !status.is_success() && status.as_u16() != 206 {
                tracing::warn!(
                    "Gap download failed with HTTP status {} ({}), cleaning up partial data",
                    status,
                    range_header
                );
                Self::cleanup_partial_gap(&mut writer, gap_start, 0).await;
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

            let mut stream = response.bytes_stream();
            let mut stream_offset = gap_start;
            let mut bytes_downloaded = 0u64;

            while let Some(chunk_result) = tokio::select! {
                chunk_result = stream.next() => chunk_result,
                cancellation = self.wait_for_cancellation() => {
                    let error = cancellation.expect_err(
                        "cancellation watcher must not complete successfully",
                    );
                    Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
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
                    Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
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
                        Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
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

                if let Some(ref lim) = limiter {
                    lim.acquire_download(data.len() as u64).await;
                }
                if let Some(ref gl) = self.global_limiter
                    && gl.is_download_limited()
                {
                    gl.acquire_download(data.len() as u64).await;
                }

                match writer.write_bytes_at(stream_offset, data.clone()).await {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            "Write failed during gap download ({}), cleaning up partial data",
                            range_header
                        );
                        Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
                        return GapDownloadResult {
                            completed_gaps,
                            error: Some(Aria2Error::Fatal(crate::error::FatalError::Config(
                                format!("Write failed: {}", e),
                            ))),
                        };
                    }
                }

                let data_len = data.len() as u64;
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
                Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
            }
        }

        if let Err(e) = writer.flush().await {
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
        self.cookie_helper.save_cookies_if_configured();
        GapDownloadResult {
            completed_gaps,
            error: None,
        }
    }

    /// Zero-fill a partially-written gap region to maintain data integrity.
    async fn cleanup_partial_gap(
        writer: &mut CachedDiskWriter,
        gap_start: u64,
        bytes_written: u64,
    ) {
        if bytes_written == 0 {
            return;
        }
        let zero_data = vec![0u8; bytes_written as usize];
        if let Err(e) = writer
            .write_bytes_at(gap_start, bytes::Bytes::from(zero_data))
            .await
        {
            tracing::warn!(
                "Failed to cleanup partial gap at {} ({} bytes): {}",
                gap_start,
                bytes_written,
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::engine::download_cookie::CookieHelper;
    use crate::engine::download_progress::ProgressUpdater;
    use crate::http::HttpRequestPolicy;
    use crate::http::cookie_storage::CookieStorage;
    use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
    use crate::util::perf_monitor::AtomicMetrics;
    use crate::util::rwlock_ext::RwLockRecover;

    #[test]
    fn classifies_416_as_range_failure() {
        assert!(matches!(
            classify_gap_http_status(416, "bytes=10-20"),
            Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable { range })
                if range == "bytes=10-20"
        ));
    }

    #[test]
    fn classifies_5xx_as_server_failure() {
        assert!(matches!(
            classify_gap_http_status(503, "bytes=10-20"),
            Aria2Error::Recoverable(RecoverableError::ServerError { code: 503 })
        ));
    }

    #[test]
    fn classifies_configured_4xx_transients_as_server_failures() {
        for status_code in [408, 429] {
            assert!(matches!(
                classify_gap_http_status(status_code, "bytes=10-20"),
                Aria2Error::Recoverable(RecoverableError::ServerError { code })
                    if code == status_code
            ));
        }
    }

    #[test]
    fn classifies_other_http_statuses_as_protocol_failures() {
        assert!(matches!(
            classify_gap_http_status(404, "bytes=10-20"),
            Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message })
                if message == "HTTP error: 404"
        ));
    }

    #[tokio::test]
    async fn gap_download_records_the_selected_peer_address() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test HTTP listener");
        let server_addr = listener.local_addr().expect("read listener address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept range request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.expect("read range request");
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 0-2/3\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\nabc",
                )
                .await
                .expect("write range response");
        });

        let dir = tempfile::tempdir().expect("create temporary download directory");
        let uri = format!("http://{server_addr}/payload.bin");
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(9_901),
            vec![uri.clone()],
            DownloadOptions::default(),
        )));
        group.recover_mut().start().expect("start request group");
        let progress = Arc::new(AtomicProgress::new());
        crate::http::client_pool::ensure_rustls_provider();
        let mut downloader = SequentialDownloader::new(
            Arc::new(
                reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .expect("build test HTTP client"),
            ),
            dir.path().join("payload.bin"),
            HttpRequestPolicy::default(),
            CookieHelper::new(Arc::new(CookieStorage::new()), None),
            ProgressUpdater::new(
                None,
                None,
                Arc::clone(&progress),
                Arc::new(AtomicMetrics::new()),
                None,
            ),
            Arc::clone(&group),
            progress,
            None,
        );

        let result = downloader.execute_with_gaps(&uri, 3, &[]).await;
        server.await.expect("test HTTP server should finish");

        assert!(
            result.error.is_none(),
            "gap download failed: {:?}",
            result.error
        );
        assert_eq!(
            tokio::fs::read(dir.path().join("payload.bin"))
                .await
                .unwrap(),
            b"abc"
        );
        let contexts = group.recover().connection_contexts();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].endpoint.hostname(), "127.0.0.1");
        assert_eq!(contexts[0].endpoint.port(), server_addr.port());
        assert_eq!(contexts[0].peer_addr, server_addr);
    }
}
