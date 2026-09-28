use futures::StreamExt;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::debug;

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::engine::http_segment_downloader::progress::SegmentProgress;
use crate::error::{Aria2Error, RecoverableError, Result};

use super::{HttpSegmentDownloader, WriteChunk, classify_range_status, validate_content_range};

impl HttpSegmentDownloader {
    /// Streaming variant of [`download_range`](Self::download_range).
    ///
    /// Instead of accumulating all chunks in memory and returning the full buffer,
    /// this method sends each chunk to `write_tx` as it arrives from the network,
    /// enabling immediate disk writes without the 16 MB per-segment memory overhead.
    ///
    /// Returns the total number of bytes downloaded on success.
    #[allow(clippy::too_many_arguments)]
    pub async fn download_range_streaming(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress_tx: Option<&mpsc::Sender<ProgressUpdate>>,
        write_tx: &mpsc::Sender<WriteChunk>,
        expected_entity_length: u64,
    ) -> Result<u64> {
        self.download_range_streaming_inner(
            url,
            offset,
            length,
            cookie_header,
            headers,
            progress_tx.map(StreamingProgress::Channel),
            write_tx,
            expected_entity_length,
        )
        .await
    }

    /// Streaming range download with lock-free progress aggregation for the
    /// concurrent HTTP scheduler.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_range_streaming_with_progress(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress: Option<&SegmentProgress>,
        write_tx: &mpsc::Sender<WriteChunk>,
        expected_entity_length: u64,
    ) -> Result<u64> {
        self.download_range_streaming_inner(
            url,
            offset,
            length,
            cookie_header,
            headers,
            progress.map(StreamingProgress::Segment),
            write_tx,
            expected_entity_length,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_range_streaming_inner(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress: Option<StreamingProgress<'_>>,
        write_tx: &mpsc::Sender<WriteChunk>,
        expected_entity_length: u64,
    ) -> Result<u64> {
        if length == 0 {
            return Ok(0);
        }

        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
        debug!("HTTP Range request (streaming): {} ({})", range_header, url);

        let collect_timing = progress.is_some() && tracing::enabled!(tracing::Level::DEBUG);
        let request_started = collect_timing.then(Instant::now);
        let (response, effective_url) = self
            .send_range_request(url, &range_header, cookie_header, headers)
            .await?;
        let response_headers_wait = request_started
            .map(|started| started.elapsed())
            .unwrap_or_default();

        self.remember_peer(response.remote_addr());
        self.remember_http_version(response.version());
        let status = response.status();
        if let Some(error) = classify_range_status(status, &range_header) {
            return Err(error);
        }
        if status.as_u16() == 206 {
            validate_content_range(&response, offset, length, expected_entity_length)?;
        }

        let mut stream = response.bytes_stream();
        let mut current_offset = offset;
        let mut total_written = 0u64;
        let mut last_reported_progress = 0u64;
        let mut response_body_wait = Duration::ZERO;
        let mut write_queue_wait = Duration::ZERO;
        let mut response_chunks = 0u64;

        loop {
            let read_started = collect_timing.then(Instant::now);
            let next_chunk = stream.next().await;
            if let Some(started) = read_started {
                response_body_wait = response_body_wait.saturating_add(started.elapsed());
            }
            let Some(chunk_result) = next_chunk else {
                break;
            };
            let bytes = chunk_result.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Stream read error: {}", e),
                })
            })?;
            response_chunks = response_chunks.saturating_add(1);
            let chunk_len = bytes.len() as u64;
            if chunk_len > 0
                && let Some(StreamingProgress::Segment(segment)) = progress
            {
                segment.record_network_activity();
            }
            if total_written.saturating_add(chunk_len) > length {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "Response exceeded requested range length: expected {}, received at least {}",
                            length,
                            total_written.saturating_add(chunk_len)
                        ),
                    },
                ));
            }

            // Send chunk to writer immediately — no accumulation
            let write_started = collect_timing.then(Instant::now);
            let write_result = write_tx
                .send(WriteChunk {
                    offset: current_offset,
                    data: bytes,
                })
                .await;
            if let Some(started) = write_started {
                write_queue_wait = write_queue_wait.saturating_add(started.elapsed());
            }
            if write_result.is_err() {
                return Err(Aria2Error::DownloadFailed(
                    "download writer channel closed".into(),
                ));
            }

            current_offset += chunk_len;
            total_written += chunk_len;

            // Report progress at the same byte threshold without scheduling a
            // receiver task for every segment.
            if progress.is_some()
                && total_written - last_reported_progress >= constants::PROGRESS_UPDATE_BYTES as u64
            {
                match progress {
                    Some(StreamingProgress::Channel(tx)) => {
                        let update = ProgressUpdate {
                            completed_bytes: offset + total_written,
                            download_speed: 0,
                            upload_speed: 0,
                        };
                        let _ = tx.send(update).await;
                    }
                    Some(StreamingProgress::Segment(segment)) => {
                        segment.record(total_written);
                    }
                    None => unreachable!("progress presence checked above"),
                }
                last_reported_progress = total_written;
            }
        }

        if total_written != length {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Incomplete response for range {}-{} from {}: expected {} bytes, received {}",
                        offset,
                        offset + length.saturating_sub(1),
                        effective_url,
                        length,
                        total_written
                    ),
                },
            ));
        }

        if collect_timing {
            debug!(
                offset,
                length,
                response_headers_wait_ms = response_headers_wait.as_millis() as u64,
                response_body_wait_ms = response_body_wait.as_millis() as u64,
                write_queue_wait_ms = write_queue_wait.as_millis() as u64,
                response_chunks,
                "HTTP Range transfer phase timing"
            );
        }

        Ok(total_written)
    }
}

#[derive(Clone, Copy)]
enum StreamingProgress<'a> {
    Channel(&'a mpsc::Sender<ProgressUpdate>),
    Segment(&'a SegmentProgress),
}
