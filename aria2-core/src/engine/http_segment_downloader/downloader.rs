//! HTTP segment downloader — range requests and streaming download logic.
//!
//! Contains the core `HttpSegmentDownloader` struct and its methods for
//! probing range support, downloading byte ranges (buffered and streaming),
//! and the `WriteChunk` type used to pipeline data to disk writers.

use bytes::BytesMut;
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::debug;

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::client_pool::ensure_rustls_provider;
use crate::http::response_processor::range::parse_content_range_value;

/// A chunk of data to be written to disk at a specific offset.
pub struct WriteChunk {
    pub offset: u64,
    pub data: bytes::Bytes,
}

pub struct HttpSegmentDownloader {
    pub client: reqwest::Client,
    last_peer_addr: std::sync::Mutex<Option<std::net::SocketAddr>>,
}

/// Validates that a partial response covers exactly the requested byte range.
fn validate_content_range(
    response: &reqwest::Response,
    offset: u64,
    length: u64,
    expected_entity_length: u64,
) -> Result<()> {
    let Some(value) = response.headers().get(reqwest::header::CONTENT_RANGE) else {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    };
    let value = value
        .to_str()
        .map_err(|_| Aria2Error::Recoverable(RecoverableError::CannotResume))?;
    let Some((start, end, total)) = parse_content_range_value(value) else {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    };
    if expected_entity_length != 0 && total != expected_entity_length {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    }
    let expected_end = offset.saturating_add(length.saturating_sub(1));
    if start != offset || end != expected_end || end < start {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    }
    Ok(())
}

impl HttpSegmentDownloader {
    /// Create a new `HttpSegmentDownloader`.
    #[must_use]
    pub fn new(client: &reqwest::Client) -> Self {
        ensure_rustls_provider();
        Self {
            client: client.clone(),
            last_peer_addr: std::sync::Mutex::new(None),
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
    pub fn remote_addr(response: &reqwest::Response) -> Option<std::net::SocketAddr> {
        response.remote_addr()
    }

    pub fn last_peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.last_peer_addr.lock().ok().and_then(|peer| *peer)
    }

    fn remember_peer(&self, peer: Option<std::net::SocketAddr>) {
        if let Ok(mut slot) = self.last_peer_addr.lock() {
            *slot = peer;
        }
    }

    pub async fn download_range(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress_tx: Option<&mpsc::UnboundedSender<ProgressUpdate>>,
        expected_entity_length: u64,
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

        self.remember_peer(response.remote_addr());
        let status = response.status();
        match status.as_u16() {
            206 => {
                validate_content_range(&response, offset, length, expected_entity_length)?;
            }
            200 => {
                return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
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
                    if data.len() as u64 + bytes.len() as u64 > length {
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!(
                                    "Response exceeded requested range length: expected {}, received more",
                                    length
                                ),
                            },
                        ));
                    }
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

        if data.len() as u64 != length {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Incomplete response for range {}-{} from {}: expected {} bytes, received {}",
                        offset,
                        offset + length.saturating_sub(1),
                        url,
                        length,
                        data.len()
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
        expected_entity_length: u64,
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

        self.remember_peer(response.remote_addr());
        let status = response.status();
        match status.as_u16() {
            206 => {
                validate_content_range(&response, offset, length, expected_entity_length)?;
            }
            200 => {
                return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
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

        if total_written != length {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Incomplete response for range {}-{} from {}: expected {} bytes, received {}",
                        offset,
                        offset + length.saturating_sub(1),
                        url,
                        length,
                        total_written
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
        ensure_rustls_provider();
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
        ensure_rustls_provider();
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl
            .download_range("http://example.com", 0, 0, None, &[], None, 0)
            .await;
        assert!(result.is_ok(), "zero-length range should return empty vec");
        assert!(result.expect("already checked ok").is_empty());
    }

    #[tokio::test]
    async fn test_downloader_creation() {
        ensure_rustls_provider();
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
        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);

        let result = dl
            .download_range(&url, 99999, 100, None, &[], None, 0)
            .await;
        assert!(result.is_err(), "416 should be an error");

        // Wait for server with timeout
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    #[tokio::test]
    async fn test_supports_range_header_parsing() {
        ensure_rustls_provider();
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
    async fn test_download_range_streaming_short_body_is_rejected() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await.expect("read should succeed");
            stream
                .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 5\r\nConnection: close\r\n\r\n01234")
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{}", addr);
        let (write_tx, _write_rx) = mpsc::unbounded_channel();

        let result = dl
            .download_range_streaming(&url, 0, 10, None, &[], None, &write_tx, 20)
            .await;
        assert!(result.is_err(), "short streaming body should be rejected");
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    #[tokio::test]
    async fn test_download_range_short_body_is_rejected() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await.expect("read should succeed");
            stream
                .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 5\r\nConnection: close\r\n\r\n01234")
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{}", addr);

        let result = dl.download_range(&url, 0, 10, None, &[], None, 20).await;
        assert!(result.is_err(), "short 206 body should be rejected");
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    #[tokio::test]
    async fn test_download_range_status_code_handling() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await.expect("read should succeed");
            stream
                .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789")
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{}", addr);

        let result = dl
            .download_range(&url, 0, 10, None, &[], None, 20)
            .await
            .expect("matching 206 should succeed");
        assert_eq!(result.as_ref(), b"0123456789");
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }
}
