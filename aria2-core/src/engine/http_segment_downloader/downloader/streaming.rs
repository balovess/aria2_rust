use futures::StreamExt;
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

        let (response, effective_url) = self
            .send_range_request(url, &range_header, cookie_header, headers)
            .await?;

        self.remember_peer(response.remote_addr());
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

        while let Some(chunk_result) = stream.next().await {
            let bytes = chunk_result.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Stream read error: {}", e),
                })
            })?;
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
            if write_tx
                .send(WriteChunk {
                    offset: current_offset,
                    data: bytes,
                })
                .await
                .is_err()
            {
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

        Ok(total_written)
    }
}

#[derive(Clone, Copy)]
enum StreamingProgress<'a> {
    Channel(&'a mpsc::Sender<ProgressUpdate>),
    Segment(&'a SegmentProgress),
}
