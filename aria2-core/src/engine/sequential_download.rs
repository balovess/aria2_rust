use std::sync::Arc;

use futures::StreamExt;
use reqwest;

use crate::constants;
use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::{CachedDiskWriter, DefaultDiskWriter, DiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::{ResumeHelper, ResumeState};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::RequestGroup;

pub struct GapDownloadResult {
    pub completed_gaps: Vec<(u64, u64)>,
    pub error: Option<Aria2Error>,
}

pub struct SequentialDownloader {
    client: Arc<reqwest::Client>,
    output_path: std::path::PathBuf,
    headers: Vec<(String, String)>,
    use_hyper: bool,
    cookie_helper: CookieHelper,
    progress_updater: ProgressUpdater,
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
}

impl SequentialDownloader {
    pub fn new(
        client: Arc<reqwest::Client>,
        output_path: std::path::PathBuf,
        headers: Vec<(String, String)>,
        use_hyper: bool,
        cookie_helper: CookieHelper,
        progress_updater: ProgressUpdater,
        group: Arc<tokio::sync::RwLock<RequestGroup>>,
    ) -> Self {
        Self {
            client,
            output_path,
            headers,
            use_hyper,
            cookie_helper,
            progress_updater,
            group,
        }
    }

    pub async fn execute(
        &mut self,
        uri: &str,
        resume_state: &ResumeState,
        total_length: u64,
    ) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        let _ = total_length;

        #[cfg(target_os = "linux")]
        {
            if !resume_state.should_resume
                && total_length > 0
                && self.use_hyper
                && !uri.starts_with("https://")
                && self.headers.is_empty()
                && self.cookie_helper.build_cookie_header(uri).is_none()
            {
                match self.try_splice_sequential(uri, total_length).await {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        tracing::debug!(
                            "Splice download failed for {}, falling back to streaming: {}",
                            uri, e
                        );
                    }
                }
            }
        }

        let url_parsed = reqwest::Url::parse(uri).ok();
        let mut request = if let Some(range_header) = ResumeHelper::build_range_header(resume_state)
        {
            tracing::debug!("Resume download: {}", range_header);
            self.client.get(uri).header("Range", range_header)
        } else {
            self.client.get(uri)
        };

        if let Some(ref url) = url_parsed {
            let cookie_hdr = self.cookie_helper.build_cookie_header_from_url(url);
            if !cookie_hdr.is_empty() {
                request = request.header("Cookie", &cookie_hdr);
            }
        }

        for (name, value) in &self.headers {
            request = request.header(name, value);
        }

        let response = request.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP request failed: {}", e),
            })
        })?;

        self.cookie_helper.extract_and_store_cookies(uri, &response);

        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            if status.as_u16() >= 500 {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                    code: status.as_u16(),
                }));
            }
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                format!("HTTP error: {}", status),
            )));
        }

        let resp_length = response.content_length().unwrap_or(0) as u64;
        let actual_total = if resume_state.should_resume {
            resume_state.start_offset + resp_length
        } else {
            resp_length
        };
        {
            let mut g = self.group.write().await;
            g.set_total_length(actual_total).await;
            g.set_total_length_atomic(actual_total);
        }

        let start_offset = if resume_state.should_resume {
            resume_state.start_offset
        } else {
            0
        };

        self.progress_updater.reset(start_offset);

        let rate_limit = { self.group.read().await.options().max_download_limit };

        let raw_writer = DefaultDiskWriter::new(&self.output_path);
        let mut writer: Box<dyn DiskWriter> = match rate_limit {
            Some(rate) if rate > 0 => {
                let cfg = RateLimiterConfig::new(Some(rate), None);
                let limiter = RateLimiter::new(&cfg);
                tracing::debug!("Download speed limit enabled: {} bytes/s", rate);
                {
                    let g = self.group.read().await;
                    g.set_rate_limiter(limiter.clone()).await;
                }
                Box::new(ThrottledWriter::new(raw_writer, limiter))
            }
            _ => Box::new(raw_writer),
        };

        let mut stream = response.bytes_stream();
        let mut completed_bytes = start_offset;
        let write_piece = constants::RATE_LIMITER_CHUNK_SIZE;

        while let Some(chunk) = stream.next().await {
            let data: bytes::Bytes = chunk.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;

            let mut offset = 0usize;
            while offset < data.len() {
                let end = (offset + write_piece).min(data.len());
                let piece = &data[offset..end];
                writer.write(piece).await?;
                completed_bytes += piece.len() as u64;
                offset = end;

                self.progress_updater
                    .update_progress(
                        completed_bytes,
                        constants::PROGRESS_UPDATE_BYTES as u64,
                        constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                    )
                    .await;
            }
        }

        writer.finalize().await.ok();

        let final_speed = {
            let g = self.group.read().await;
            let elapsed = g.elapsed_time().await;
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (completed_bytes as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            g.set_completed_length(completed_bytes);
            g.set_download_speed_cached(final_speed);
            g.complete().await?;
        }

        tracing::info!(
            "Sequential download complete: {} ({} bytes)",
            self.output_path.display(),
            completed_bytes
        );
        self.cookie_helper.save_cookies_if_configured();
        Ok(())
    }

    pub async fn execute_with_gaps(
        &mut self,
        uri: &str,
        total_length: u64,
        completed_ranges: &[(u64, u64)],
    ) -> GapDownloadResult {
        let gaps = Self::find_all_gaps(completed_ranges, total_length);
        tracing::info!(
            "Starting sequential download with gaps: uri={}, total={}, gaps={:?}",
            uri, total_length, gaps
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

        let mut writer = CachedDiskWriter::new(&self.output_path, Some(total_length), None);

        let rate_limit = { self.group.read().await.options().max_download_limit };
        let limiter = rate_limit
            .filter(|&r| r > 0)
            .map(|r| RateLimiter::new(&RateLimiterConfig::new(Some(r), None)));
        if let Some(ref lim) = limiter {
            let g = self.group.read().await;
            g.set_rate_limiter(lim.clone()).await;
        }

        let mut last_progress_update = completed_bytes;
        let mut completed_gaps: Vec<(u64, u64)> = Vec::new();

        for (gap_start, gap_length) in gaps {
            let gap_end = gap_start + gap_length - 1;
            let range_header = format!("bytes={}-{}", gap_start, gap_end);
            tracing::debug!("Sequential Range request for gap: {}", range_header);

            let mut request = self.client.get(uri).header("Range", &range_header);
            if let Some(ref hdr) = cookie_hdr {
                request = request.header("Cookie", hdr);
            }
            for (name, value) in &self.headers {
                request = request.header(name, value);
            }

            let response = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "Gap download failed ({}), cleaning up partial data",
                        range_header
                    );
                    Self::cleanup_partial_gap(&mut writer, gap_start, 0).await;
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            message: format!("HTTP request failed: {}", e),
                        })),
                    };
                }
            };

            self.cookie_helper.extract_and_store_cookies(uri, &response);

            let status = response.status();
            if !status.is_success() && status.as_u16() != 206 {
                tracing::warn!(
                    "Gap download failed with HTTP status {} ({}), cleaning up partial data",
                    status, range_header
                );
                Self::cleanup_partial_gap(&mut writer, gap_start, 0).await;
                let error = if status.as_u16() >= 500 {
                    Aria2Error::Recoverable(RecoverableError::ServerError {
                        code: status.as_u16(),
                    })
                } else {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        format!("HTTP error: {}", status),
                    ))
                };
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(error),
                };
            }

            let mut stream = response.bytes_stream();
            let mut stream_offset = gap_start;
            let mut bytes_downloaded = 0u64;

            while let Some(chunk_result) = stream.next().await {
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
                            error: Some(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                                message: e.to_string(),
                            })),
                        };
                    }
                };

                if let Some(ref lim) = limiter {
                    lim.acquire_download(data.len() as u64).await;
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
                            error: Some(Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                                "Write failed: {}", e
                            )))),
                        };
                    }
                }

                let data_len = data.len() as u64;
                completed_bytes += data_len;
                stream_offset += data_len;
                bytes_downloaded += data_len;

                if completed_bytes - last_progress_update >= constants::PROGRESS_UPDATE_BYTES as u64 {
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
                error: Some(Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Flush failed: {}", e
                )))),
            };
        }

        let final_speed = {
            let g = self.group.read().await;
            let elapsed = g.elapsed_time().await;
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (completed_bytes as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };
        {
            let mut g = self.group.write().await;
            g.set_total_length(completed_bytes).await;
            g.set_total_length_atomic(completed_bytes);
            g.update_progress(completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            g.set_completed_length(completed_bytes);
            g.set_download_speed_cached(final_speed);
            if let Err(e) = g.complete().await {
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

    async fn cleanup_partial_gap(writer: &mut CachedDiskWriter, gap_start: u64, bytes_written: u64) {
        if bytes_written == 0 {
            return;
        }
        let zero_data = vec![0u8; bytes_written as usize];
        if let Err(e) = writer.write_bytes_at(gap_start, bytes::Bytes::from(zero_data)).await {
            tracing::warn!("Failed to cleanup partial gap at {} ({} bytes): {}", gap_start, bytes_written, e);
        }
    }

    pub fn merge_ranges(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
        if ranges.is_empty() {
            return Vec::new();
        }

        let mut sorted = ranges.to_vec();
        sorted.sort_by_key(|r| r.0);

        let mut merged = Vec::new();
        let mut current = sorted[0];

        for &(offset, length) in sorted.iter().skip(1) {
            let current_end = current.0 + current.1;
            let next_end = offset + length;

            if offset <= current_end {
                current = (current.0, std::cmp::max(current_end, next_end) - current.0);
            } else {
                merged.push(current);
                current = (offset, length);
            }
        }
        merged.push(current);
        merged
    }

    pub fn find_all_gaps(completed_ranges: &[(u64, u64)], total_length: u64) -> Vec<(u64, u64)> {
        let merged_ranges = Self::merge_ranges(completed_ranges);
        let mut gaps = Vec::new();
        if merged_ranges.is_empty() {
            if total_length > 0 {
                gaps.push((0, total_length));
            }
            return gaps;
        }

        let mut current = 0;
        for &(offset, length) in &merged_ranges {
            if offset > current {
                gaps.push((current, offset - current));
            }
            current = std::cmp::max(current, offset + length);
        }
        if current < total_length {
            gaps.push((current, total_length - current));
        }
        gaps
    }

    pub async fn execute_with_gaps_with_retry(
        &mut self,
        uri: &str,
        total_length: u64,
        completed_ranges: &[(u64, u64)],
        retry_policy: &RetryPolicy,
    ) -> Result<()> {
        let mut last_err = None;
        let mut accumulated_completed: Vec<(u64, u64)> = completed_ranges.to_vec();

        for attempt in 0..=retry_policy.max_retries {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt - 1)
            {
                tracing::info!(
                    "Sequential download with gaps retry #{} (waiting {:?}), {} ranges already completed...",
                    attempt, wait, accumulated_completed.len()
                );
                tokio::time::sleep(wait).await;
            }

            let result = self.execute_with_gaps(uri, total_length, &accumulated_completed).await;

            if !result.completed_gaps.is_empty() {
                tracing::info!(
                    "Attempt #{} completed {} gaps",
                    attempt + 1,
                    result.completed_gaps.len()
                );
                accumulated_completed.extend(result.completed_gaps);
                accumulated_completed = Self::merge_ranges(&accumulated_completed);
            }

            if result.error.is_none() {
                return Ok(());
            }

            tracing::warn!("Sequential download with gaps attempt #{} failed: {}", attempt + 1, result.error.as_ref().unwrap());
            last_err = result.error;

            if retry_policy.is_exhausted(attempt)
                || !retry_policy
                    .should_retry_error(&format!("{:?}", last_err.as_ref().unwrap()))
            {
                break;
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "All retries failed".into(),
            })
        }))
    }

    pub async fn execute_with_retry(
        &mut self,
        uri: &str,
        resume_state: &ResumeState,
        total_length: u64,
        retry_policy: &RetryPolicy,
    ) -> Result<()> {
        let mut last_err = None;

        for attempt in 0..=retry_policy.max_retries {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt - 1)
            {
                tracing::info!(
                    "Sequential download retry #{} (waiting {:?})...",
                    attempt, wait
                );
                tokio::time::sleep(wait).await;
            }

            match self.execute(uri, resume_state, total_length).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("Sequential download attempt #{} failed: {}", attempt + 1, e);
                    last_err = Some(e);
                    if retry_policy.is_exhausted(attempt)
                        || !retry_policy
                            .should_retry_error(&format!("{:?}", last_err.as_ref().unwrap()))
                    {
                        break;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "All retries failed".into(),
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::SequentialDownloader;

    #[test]
    fn test_merge_ranges_empty() {
        let ranges = &[];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert!(result.is_empty());
    }

    #[test]
    fn test_merge_ranges_single() {
        let ranges = &[(0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100)]);
    }

    #[test]
    fn test_merge_ranges_non_overlapping_sorted() {
        let ranges = &[(0, 100), (200, 100), (400, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100), (200, 100), (400, 100)]);
    }

    #[test]
    fn test_merge_ranges_non_overlapping_unsorted() {
        let ranges = &[(200, 100), (0, 100), (400, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100), (200, 100), (400, 100)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_inner() {
        let ranges = &[(0, 200), (50, 50)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_inner_unsorted() {
        let ranges = &[(50, 50), (0, 200)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_partial() {
        let ranges = &[(0, 100), (50, 150)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_partial_unsorted() {
        let ranges = &[(50, 150), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_adjacent() {
        let ranges = &[(0, 100), (100, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_adjacent_unsorted() {
        let ranges = &[(100, 100), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_duplicate() {
        let ranges = &[(0, 100), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100)]);
    }

    #[test]
    fn test_merge_ranges_multiple_overlapping() {
        let ranges = &[(0, 100), (50, 150), (200, 100), (180, 150)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 330)]);
    }

    #[test]
    fn test_merge_ranges_zero_length() {
        let ranges = &[(0, 0), (100, 0), (200, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 0), (100, 0), (200, 100)]);
    }

    #[test]
    fn test_merge_ranges_complex() {
        let ranges = &[(10, 5), (0, 20), (15, 25), (50, 10), (45, 20), (100, 50)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 40), (45, 20), (100, 50)]);
    }

    #[test]
    fn test_find_all_gaps_empty_ranges() {
        let ranges = &[];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(0, 1000)]);
    }

    #[test]
    fn test_find_all_gaps_no_gaps() {
        let ranges = &[(0, 1000)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert!(gaps.is_empty());
    }

    #[test]
    fn test_find_all_gaps_single_gap() {
        let ranges = &[(0, 500), (600, 400)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(500, 100)]);
    }

    #[test]
    fn test_find_all_gaps_multiple_gaps() {
        let ranges = &[(0, 100), (200, 100), (400, 200)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(100, 100), (300, 100), (600, 400)]);
    }

    #[test]
    fn test_find_all_gaps_overlapping_ranges() {
        let ranges = &[(0, 200), (100, 150), (300, 50)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 500);
        assert_eq!(gaps, vec![(250, 50), (350, 150)]);
    }
}

impl SequentialDownloader {
    #[cfg(target_os = "linux")]
    async fn try_splice_sequential(&mut self, uri: &str, total_length: u64) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.output_path)?;

        let bytes = crate::http::splice_http::try_splice_download(uri, 0, total_length, &file, 0)
            .await
            .map_err(|e| Aria2Error::Io(format!("splice download failed: {e}")))?;

        let final_speed = {
            let g = self.group.read().await;
            let elapsed = g.elapsed_time().await;
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => (bytes as f64 / d.as_secs_f64()) as u64,
                _ => 0,
            }
        };

        {
            let mut g = self.group.write().await;
            g.set_total_length(bytes).await;
            g.set_total_length_atomic(bytes);
            g.update_progress(bytes).await;
            g.set_completed_length(bytes);
            g.update_speed(final_speed, 0).await;
            g.set_download_speed_cached(final_speed);
            g.complete().await?;
        }

        tracing::info!(
            "Sequential download (splice) complete: {} ({} bytes)",
            self.output_path.display(),
            bytes
        );
        self.cookie_helper.save_cookies_if_configured();
        Ok(())
    }
}
