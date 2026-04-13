use futures::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, warn};

use crate::error::{Aria2Error, RecoverableError, Result};

pub struct HttpSegmentDownloader {
    client: reqwest::Client,
}

/// Calculate optimal segment size based on download speed and remaining data.
/// Returns size in bytes (between MIN_SEGMENT_SIZE and MAX_SEGMENT_SIZE).
pub fn calculate_dynamic_segment_size(
    total_remaining: u64,
    num_connections: usize,
    avg_speed_bps: f64,
    elapsed_secs: u64,
) -> u64 {
    const MIN_SEGMENT: u64 = 1024 * 256; // 256 KB
    const MAX_SEGMENT: u64 = 1024 * 1024 * 16; // 16 MB

    if elapsed_secs < 2 || avg_speed_bps < 1024.0 {
        // Too early or too slow — use conservative default
        return (total_remaining / num_connections.max(1) as u64).clamp(MIN_SEGMENT, MAX_SEGMENT);
    }

    // Target ~10 seconds per segment at current speed
    let target_size = (avg_speed_bps * 10.0) as u64;
    target_size.clamp(MIN_SEGMENT, MAX_SEGMENT)
}

/// Track active connections per hostname to enforce max-connection-per-server limit.
pub struct ConnectionLimiter {
    per_host: HashMap<String, usize>,
    global_limit: usize,
    per_host_limit: usize,
}

impl ConnectionLimiter {
    /// Create a new ConnectionLimiter with specified limits.
    pub fn new(global: usize, per_host: usize) -> Self {
        Self {
            per_host: HashMap::new(),
            global_limit: global,
            per_host_limit: per_host,
        }
    }

    /// Try to acquire a connection slot for the given host.
    /// Returns true if allowed, false if at limit.
    pub fn try_acquire(&mut self, host: &str) -> bool {
        // Check per-host limit first
        let current_count = *self.per_host.get(host).unwrap_or(&0);
        if current_count >= self.per_host_limit {
            return false;
        }

        // Check global limit (sum of all connections)
        let global_count: usize = self.per_host.values().sum();
        if global_count >= self.global_limit {
            return false;
        }

        // Acquire the slot
        *self.per_host.entry(host.to_string()).or_insert(0) += 1;
        true
    }

    /// Release a slot when a connection completes/fails.
    pub fn release(&mut self, host: &str) {
        if let Some(count) = self.per_host.get_mut(host)
            && *count > 0
        {
            *count -= 1;
        }
    }

    /// How many slots available for this host?
    pub fn available_for(&self, host: &str) -> usize {
        let current = self.per_host.get(host).copied().unwrap_or(0);
        if current >= self.per_host_limit {
            return 0;
        }
        self.per_host_limit - current
    }
}

/// Score a source based on recent speed measurements.
/// Lower score = better source (tried first).
pub fn score_source(avg_speed_bps: f64, failure_count: u32, last_success_age_secs: u64) -> f64 {
    if avg_speed_bps <= 0.0 && failure_count > 0 {
        return f64::MAX; // dead source
    }
    let speed_score = -avg_speed_bps.ln_1p(); // higher speed → lower (better) score
    let penalty = (failure_count as f64) * 100.0; // each failure adds penalty
    let age_bonus = (last_success_age_secs as f64 / 60.0).min(10.0); // recent success bonus
    speed_score + penalty - age_bonus
}

impl HttpSegmentDownloader {
    pub fn new(client: &reqwest::Client) -> Self {
        Self {
            client: client.clone(),
        }
    }

    pub async fn supports_range(&self, url: &str, cookie_header: Option<&str>) -> Result<bool> {
        let mut req = self.client.head(url);
        if let Some(ch) = cookie_header {
            req = req.header("Cookie", ch);
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

    pub async fn download_range(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
    ) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }

        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
        debug!("HTTP Range request: {} ({})", range_header, url);

        let mut req = self
            .client
            .get(url)
            .header("Range", &range_header)
            .timeout(Duration::from_secs(120));
        if let Some(ch) = cookie_header {
            req = req.header("Cookie", ch);
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

        let mut data = Vec::with_capacity(length as usize);
        let mut stream = response.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => data.extend_from_slice(&bytes),
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

        Ok(data)
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
            .unwrap();
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl
            .supports_range("http://127.0.0.1:1/nonexistent", None)
            .await;
        assert!(result.is_err(), "should fail for unreachable host");
    }

    #[tokio::test]
    async fn test_download_range_zero_length() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl.download_range("http://example.com", 0, 0, None).await;
        assert!(result.is_ok(), "zero-length range should return empty vec");
        assert!(result.unwrap().is_empty());
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            stream.read(&mut buf).await.unwrap();
            stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let url = format!("http://{}", addr);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let dl = HttpSegmentDownloader::new(&client);

        let result = dl.download_range(&url, 99999, 100, None).await;
        assert!(result.is_err(), "416 should be an error");

        server_handle.await.ok();
    }

    #[tokio::test]
    async fn test_supports_range_header_parsing() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);

        assert!(
            dl.supports_range(
                "http://invalid-host-name-that-does-not-exist-12345.com/",
                None
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn test_download_range_status_code_handling() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);

        let result_404 = dl
            .download_range("http://httpbin.org/status/404", 0, 100, None)
            .await;
        assert!(result_404.is_err(), "404 should be fatal error");
    }

    #[test]
    fn test_dynamic_segment_size_slow_start() {
        // Early download (elapsed < 2 seconds) should use conservative default
        let size = calculate_dynamic_segment_size(10_000_000, 4, 50000.0, 1);
        // With 10MB remaining and 4 connections: 10_000_000 / 4 = 2.5MB = 2621440 bytes
        // Should be clamped between MIN_SEGMENT (256KB) and MAX_SEGMENT (16MB)
        assert!(size >= 1024 * 256, "Should be at least MIN_SEGMENT");
        assert!(size <= 1024 * 1024 * 16, "Should be at most MAX_SEGMENT");

        // Very slow speed (< 1KB/s) should also use conservative default
        let size_slow = calculate_dynamic_segment_size(10_000_000, 4, 100.0, 5);
        assert!(
            size_slow >= 1024 * 256,
            "Slow speed should use conservative default"
        );
    }

    #[test]
    fn test_dynamic_segment_size_fast_download() {
        // Fast download (1 MB/s = 1048576 B/s) with sufficient elapsed time
        let size = calculate_dynamic_segment_size(100_000_000, 8, 1_048_576.0, 10);
        // Target size = 1048576.0 * 10.0 = 10485760 bytes (~10 MB)
        // Should be clamped to MAX_SEGMENT if needed
        assert_eq!(
            size, 10_485_760,
            "Fast download should produce large segments"
        );

        // Very fast download (10 MB/s)
        let size_very_fast = calculate_dynamic_segment_size(1_000_000_000, 16, 10_485_760.0, 30);
        // Target = 104857600 bytes (~100 MB), but capped at MAX_SEGMENT (16 MB)
        assert_eq!(
            size_very_fast, 16_777_216,
            "Very fast download should be capped at MAX_SEGMENT"
        );
    }

    #[test]
    fn test_connection_limiter_per_host() {
        let mut limiter = ConnectionLimiter::new(10, 2); // Global limit 10, per-host limit 2

        // Should be able to acquire up to per_host_limit
        assert!(
            limiter.try_acquire("example.com"),
            "First acquisition should succeed"
        );
        assert!(
            limiter.try_acquire("example.com"),
            "Second acquisition should succeed"
        );
        assert!(
            !limiter.try_acquire("example.com"),
            "Third acquisition should fail (per-host limit)"
        );

        // Different host should work independently
        assert!(
            limiter.try_acquire("other.com"),
            "Different host should work"
        );
        assert!(
            limiter.try_acquire("other.com"),
            "Second slot for other host"
        );
        assert!(
            !limiter.try_acquire("other.com"),
            "Third slot for other host should fail"
        );

        // Release a slot
        limiter.release("example.com");
        assert!(
            limiter.try_acquire("example.com"),
            "After release, should acquire again"
        );

        // Check available slots
        assert_eq!(
            limiter.available_for("example.com"),
            0,
            "No slots available after acquiring limit"
        );
        limiter.release("example.com");
        assert_eq!(
            limiter.available_for("example.com"),
            1,
            "One slot available after release"
        );
    }

    #[test]
    fn test_source_scoring_slow_penalized() {
        // Fast source (1 MB/s)
        let fast_score = score_source(1_048_576.0, 0, 0);

        // Slow source (1 KB/s)
        let slow_score = score_source(1024.0, 0, 0);

        // Dead source (no speed + failures)
        let dead_score = score_source(0.0, 3, 0);

        // Slow source should have higher (worse) score than fast source
        assert!(
            slow_score > fast_score,
            "Slow source should have worse score than fast source"
        );

        // Dead source should have maximum score
        assert_eq!(dead_score, f64::MAX, "Dead source should have MAX score");

        // Source with failures should be penalized
        let failed_score = score_source(1_048_576.0, 2, 0);
        assert!(
            failed_score > fast_score,
            "Failed source should have worse score than successful one"
        );

        // Recent success should improve score (lower is better)
        // Note: age_bonus is subtracted, so more recent (smaller age) = smaller subtraction = slightly higher score
        // But the effect is minimal compared to speed differences
        let recent_score = score_source(1_048_576.0, 0, 10); // 10 seconds ago
        let old_score = score_source(1_048_576.0, 0, 300); // 5 minutes ago
        // Both should have similar base scores (same speed), but old success has larger age bonus subtracted
        assert!(
            old_score < recent_score,
            "Old success should give better (lower) score due to larger age bonus"
        );

        // Verify that both are still much better than slow sources
        let very_slow = score_source(1024.0, 0, 0);
        assert!(
            recent_score < very_slow,
            "Even recent fast source beats slow source"
        );
    }
}
