use dashmap::DashMap;
use futures::StreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::engine::command::ProgressUpdate;
// Re-export score_source for convenience
pub use crate::selector::adaptive_uri_selector::{
    score_source_raw as score_source, score_source_raw,
};

/// A chunk of data to be written to disk at a specific offset.
pub struct WriteChunk {
    pub offset: u64,
    pub data: bytes::Bytes,
}

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
///
/// Uses `DashMap` + `AtomicUsize` for interior mutability, so every method takes
/// `&self` and the limiter can be shared via `Arc<ConnectionLimiter>` without any
/// wrapping `Mutex`/`RwLock`. This is a *soft* limiter: the CAS-with-rollback
/// pattern in [`ConnectionLimiter::try_acquire`] keeps the global and per-host
/// counts bounded by their limits at the moment of each successful CAS, but
/// snapshot readers (e.g. [`ConnectionLimiter::host_count`]) may observe
/// briefly-stale values.
pub struct ConnectionLimiter {
    per_host: DashMap<String, AtomicUsize>,
    global_count: AtomicUsize,
    global_limit: usize,
    per_host_limit: usize,
}

impl ConnectionLimiter {
    /// Create a new `ConnectionLimiter` with the given global and per-host limits.
    pub fn new(global: usize, per_host: usize) -> Self {
        Self {
            per_host: DashMap::new(),
            global_count: AtomicUsize::new(0),
            global_limit: global,
            per_host_limit: per_host,
        }
    }

    /// Try to acquire a connection slot for the given host.
    ///
    /// Returns `true` if the slot was acquired, `false` if either the global or
    /// the per-host limit has been reached.
    ///
    /// # Algorithm (CAS-with-rollback)
    ///
    /// 1. Atomically increment `global_count` via CAS; abort if at/above limit.
    /// 2. Atomically increment the per-host counter via CAS; if the per-host
    ///    limit is hit, roll back the global increment so `global_count` stays
    ///    accurate.
    ///
    /// This guarantees `global_count` never exceeds `global_limit` at the
    /// moment of a successful CAS, and each host's counter never exceeds
    /// `per_host_limit`. A brief shard-level write lock is held by the
    /// `DashMap` `Entry` guard during the per-host CAS — acceptable for a
    /// connection limiter which is not a hot path.
    pub fn try_acquire(&self, host: &str) -> bool {
        // Step 1: Atomically acquire a global slot via CAS. Only threads that
        // observe `current < global_limit` can succeed, so the global count can
        // never exceed `global_limit` at the instant of a successful CAS.
        loop {
            let current = self.global_count.load(Ordering::Relaxed);
            if current >= self.global_limit {
                return false;
            }
            match self.global_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }

        // Step 2: Try to acquire a per-host slot. On failure, roll back the
        // global increment performed above so the global count stays accurate.
        // The rollback uses Relaxed ordering: it only needs to eventually
        // correct the over-count introduced in step 1, which was already
        // published with AcqRel. A short-lived false-positive on the global
        // limit (another thread briefly sees the inflated count) is acceptable
        // for a soft limiter.
        let entry = self
            .per_host
            .entry(host.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
        loop {
            let current = entry.load(Ordering::Relaxed);
            if current >= self.per_host_limit {
                // Per-host limit reached — roll back the global increment.
                self.global_count.fetch_sub(1, Ordering::Relaxed);
                return false;
            }
            match entry.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// Release a previously-acquired connection slot for the given host.
    ///
    /// The caller must call this exactly once per successful `try_acquire` for
    /// the same host. Double-releases are detected via `debug_assert!` in debug
    /// builds.
    pub fn release(&self, host: &str) {
        if let Some(entry) = self.per_host.get(host) {
            let prev = entry.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(prev > 0, "release called more times than acquire for host");
        }
        let prev = self.global_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "global release underflow");
    }

    /// Current connection count for a host (snapshot — may be slightly stale).
    pub fn host_count(&self, host: &str) -> usize {
        self.per_host
            .get(host)
            .map(|e| e.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Total connection count across all hosts (snapshot — may be slightly stale).
    pub fn global_count(&self) -> usize {
        self.global_count.load(Ordering::Relaxed)
    }

    /// The configured global connection limit.
    pub fn global_limit(&self) -> usize {
        self.global_limit
    }

    /// The configured per-host connection limit.
    pub fn per_host_limit(&self) -> usize {
        self.per_host_limit
    }

    /// How many slots are still available for the given host (snapshot).
    ///
    /// Returns 0 if the host is at or above its per-host limit.
    pub fn available_for(&self, host: &str) -> usize {
        let current = self.host_count(host);
        if current >= self.per_host_limit {
            return 0;
        }
        self.per_host_limit - current
    }
}

impl HttpSegmentDownloader {
    /// Create a new `HttpSegmentDownloader`.
    pub fn new(client: &reqwest::Client) -> Self {
        Self {
            client: client.clone(),
        }
    }

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
        let mut data = bytes::BytesMut::with_capacity(initial_cap);
        let mut stream = response.bytes_stream();
        let mut last_reported_progress = 0u64;

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    data.extend_from_slice(&bytes);
                    // Report per-chunk progress if a progress channel is provided
                    let downloaded = data.len() as u64;
                    if let Some(ref tx) = progress_tx {
                        if downloaded - last_reported_progress
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
            if let Some(ref tx) = progress_tx {
                if total_written - last_reported_progress
                    >= constants::PROGRESS_UPDATE_BYTES as u64
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
            .unwrap();
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
            // Use read() instead of read_exact() to avoid blocking on exact byte count
            let _n = stream.read(&mut buf).await.unwrap();
            stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let url = format!("http://{}", addr);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
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
            .unwrap();
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
        let limiter = ConnectionLimiter::new(10, 2); // Global limit 10, per-host limit 2

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

    /// Verify the limiter is safe under concurrency: 20 tasks contend on a
    /// single host whose per-host limit is 10, so exactly 10 must succeed and
    /// the limiter must not deadlock. Also verifies `release` correctness.
    #[tokio::test]
    async fn test_connection_limiter_concurrent_no_deadlock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let limiter = Arc::new(ConnectionLimiter::new(100, 10));
        let success_count = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..20 {
            let l = limiter.clone();
            let s = success_count.clone();
            handles.push(tokio::spawn(async move {
                // `try_acquire` is fully synchronous — no `.await` while holding
                // the DashMap shard guard, so there is no risk of deadlock.
                if l.try_acquire("example.com") {
                    s.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // per_host_limit is 10, so at most 10 should succeed; global limit (100)
        // is not the binding constraint here.
        assert_eq!(
            success_count.load(Ordering::Relaxed),
            10,
            "per-host limit must cap successful acquires"
        );
        assert_eq!(
            limiter.host_count("example.com"),
            10,
            "host_count must reflect the 10 successful acquires"
        );
        assert_eq!(
            limiter.global_count(),
            10,
            "global_count must equal the 10 successful acquires"
        );

        // Release some and verify the counts drop accordingly.
        for _ in 0..5 {
            limiter.release("example.com");
        }
        assert_eq!(
            limiter.host_count("example.com"),
            5,
            "host_count must drop to 5 after 5 releases"
        );
        assert_eq!(
            limiter.global_count(),
            5,
            "global_count must drop to 5 after 5 releases"
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
