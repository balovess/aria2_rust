//! HTTP segment downloader — range requests and streaming download logic.
//!
//! Contains the core `HttpSegmentDownloader` struct and its methods for
//! probing range support, downloading byte ranges (buffered and streaming),
//! and the `WriteChunk` type used to pipeline data to disk writers.

use bytes::BytesMut;
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::error::{Aria2Error, RecoverableError, Result};

/// A chunk of data to be written to disk at a specific offset.
pub struct WriteChunk {
    pub offset: u64,
    pub data: bytes::Bytes,
}

pub struct HttpSegmentDownloader {
    pub client: reqwest::Client,
}

impl HttpSegmentDownloader {
    /// Create a new `HttpSegmentDownloader`.
    pub fn new(client: &reqwest::Client) -> Self {
        Self {
            client: client.clone(),
        }
    }

    /// Probe whether the server supports byte-range requests.
    pub async fn supports_range(
        &self,
        url: &str,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
    ) -> Result<bool> {
        let mut req = self.client.head(url);
        if let Some(ch) = cookie_header {
            req = req.header("Cookie", ch);
        }
        for (name, value) in headers {
            req = req.header(name, value);
        }
        let resp = req.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HEAD request failed: {}", e),
            })
        })?;

        if let Some(accept_ranges) = resp.headers().get("Accept-Ranges")
            && let Ok(value) = accept_ranges.to_str()
        {
            return Ok(value.to_lowercase().contains("bytes"));
        }

        let status = resp.status();
        if status.as_u16() >= 400 {
            return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: status.as_u16(),
            }));
        }

        Ok(false)
    }

    /// Download a byte range from a URL.
    ///
    /// Downloads the specified byte range from the given URL. If `progress_tx` is
    /// provided, periodic progress updates (segment-relative bytes downloaded) will
    /// be sent through the channel, enabling smooth progress reporting for RPC clients.
    pub async fn download_range(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress_tx: Option<&mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<bytes::Bytes> {
        if length == 0 {
            return Ok(bytes::Bytes::new());
        }

        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
        debug!("HTTP Range request: {} ({})", range_header, url);

        let mut req =
            self.client
                .get(url)
                .header("Range", &range_header)
                .timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                ));
        if let Some(ch) = cookie_header {
            req = req.header("Cookie", ch);
        }
        for (name, value) in headers {
            req = req.header(name, value);
        }
        let response = req.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP Range request failed: {}", e),
            })
        })?;

        let status = response.status();
        match status.as_u16() {
            206 => {}
            200 => {
                warn!(
                    "Server returned 200 instead of 206 for Range request (offset={}, len={}), reading full body",
                    offset, length
                );
            }
            416 => {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "Range not satisfiable: bytes={}-{}",
                            offset,
                            offset + length.saturating_sub(1)
                        ),
                    },
                ));
            }
            code if (400..500).contains(&code) => {
                return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("HTTP client error {}: {}", code, url),
                )));
            }
            code if code >= 500 => {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                    code,
                }));
            }
            _ => {}
        }

        // Don't pre-allocate the full segment length — it can be very large
        // (16 MB+) and wastes memory if the download fails early.  Start with
        // a reasonable chunk size and let BytesMut grow organically.
        let initial_cap = (length as usize).min(256 * 1024);
        let mut data = BytesMut::with_capacity(initial_cap);
        let mut stream = response.bytes_stream();
        let mut last_reported_progress = 0u64;

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    data.extend_from_slice(&bytes);
                    // Report per-chunk progress if a progress channel is provided
                    let downloaded = data.len() as u64;
                    if let Some(tx) = progress_tx
                        && downloaded - last_reported_progress
                            >= constants::PROGRESS_UPDATE_BYTES as u64
                        {
                            let update = ProgressUpdate {
                                completed_bytes: offset + downloaded,
                                download_speed: 0,
                                upload_speed: 0,
                            };
                            let _ = tx.send(update);
                            last_reported_progress = downloaded;
                        }
                }
                Err(e) => {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!("Stream read error: {}", e),
                        },
                    ));
                }
            }
        }

        if data.is_empty() && length > 0 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Empty response for range {}-{} from {}",
                        offset,
                        offset + length.saturating_sub(1),
                        url
                    ),
                },
            ));
        }

        // Freeze BytesMut to immutable Bytes (zero-cost conversion)
        Ok(data.freeze())
    }

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
        progress_tx: Option<&mpsc::UnboundedSender<ProgressUpdate>>,
        write_tx: &mpsc::UnboundedSender<WriteChunk>,
    ) -> Result<u64> {
        if length == 0 {
            return Ok(0);
        }

        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
        debug!("HTTP Range request (streaming): {} ({})", range_header, url);

        let mut req =
            self.client
                .get(url)
                .header("Range", &range_header)
                .timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                ));
        if let Some(ch) = cookie_header {
            req = req.header("Cookie", ch);
        }
        for (name, value) in headers {
            req = req.header(name, value);
        }
        let response = req.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP Range request failed: {}", e),
            })
        })?;

        let status = response.status();
        match status.as_u16() {
            206 => {}
            200 => {
                warn!(
                    "Server returned 200 instead of 206 for Range request (offset={}, len={}), reading full body",
                    offset, length
                );
            }
            416 => {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::RangeNotSatisfiable {
                        range: format!("bytes={}-{}", offset, offset + length.saturating_sub(1)),
                    },
                ));
            }
            code if (400..500).contains(&code) => {
                return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("HTTP client error {}: {}", code, url),
                )));
            }
            code if code >= 500 => {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                    code,
                }));
            }
            _ => {}
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

            // Send chunk to writer immediately — no accumulation
            let _ = write_tx.send(WriteChunk {
                offset: current_offset,
                data: bytes,
            });

            current_offset += chunk_len;
            total_written += chunk_len;

            // Report per-chunk progress if a progress channel is provided
            if let Some(tx) = progress_tx
                && total_written - last_reported_progress >= constants::PROGRESS_UPDATE_BYTES as u64
                {
                    let update = ProgressUpdate {
                        completed_bytes: offset + total_written,
                        download_speed: 0,
                        upload_speed: 0,
                    };
                    let _ = tx.send(update);
                    last_reported_progress = total_written;
                }
        }

        if total_written == 0 && length > 0 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Empty response for range {}-{} from {}",
                        offset,
                        offset + length.saturating_sub(1),
                        url
                    ),
                },
            ));
        }

        Ok(total_written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_supports_range_no_server() {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(100))
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl
            .supports_range("http://127.0.0.1:1/nonexistent", None, &[])
            .await;
        assert!(result.is_err(), "should fail for unreachable host");
    }

    #[tokio::test]
    async fn test_download_range_zero_length() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl
            .download_range("http://example.com", 0, 0, None, &[], None)
            .await;
        assert!(result.is_ok(), "zero-length range should return empty vec");
        assert!(result.expect("already checked ok").is_empty());
    }

    #[tokio::test]
    async fn test_downloader_creation() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);
        let _dl2 = HttpSegmentDownloader::new(&dl.client);
    }

    #[tokio::test]
    async fn test_download_range_with_mock_http_416() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");

        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buf = [0u8; 2048];
            // Use read() instead of read_exact() to avoid blocking on exact byte count
            let _n = stream.read(&mut buf).await.expect("read should succeed");
            stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.expect("write should succeed");
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let url = format!("http://{}", addr);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);

        let result = dl.download_range(&url, 99999, 100, None, &[], None).await;
        assert!(result.is_err(), "416 should be an error");

        // Wait for server with timeout
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    #[tokio::test]
    async fn test_supports_range_header_parsing() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);

        match dl
            .supports_range(
                "http://invalid-host-name-that-does-not-exist-12345.com/",
                None,
                &[],
            )
            .await
        {
            Ok(supports) => {
                eprintln!(
                    "[WARN] Unexpected success for invalid host, supports={:?}",
                    supports
                );
            }
            Err(e) => {
                println!("Expected network error for invalid host: {:?}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_download_range_status_code_handling() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);

        let result_404 = dl
            .download_range("http://httpbin.org/status/404", 0, 100, None, &[], None)
            .await;
        assert!(result_404.is_err(), "404 should be fatal error");
    }
}
