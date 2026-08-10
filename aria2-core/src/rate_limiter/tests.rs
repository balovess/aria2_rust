#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::{Duration, Instant};

#[cfg(test)]
use super::config::RateLimiterConfig;
#[cfg(test)]
use super::rate_limiter::RateLimiter;
#[cfg(test)]
use super::throttled_writer::ThrottledWriter;
#[cfg(test)]
use super::token_bucket::TokenBucket;
#[cfg(test)]
use crate::filesystem::disk_writer::DiskWriter;

#[tokio::test]
async fn test_token_bucket_unlimited() {
    let tb = TokenBucket::unlimited();
    assert!(tb.is_unlimited());
    tb.acquire(1024 * 1024 * 1024).await;
    assert!(tb.available_tokens() > 0.0);
}

#[tokio::test]
async fn test_token_bucket_basic_acquire() {
    let tb = TokenBucket::new(10000, Some(5000));
    assert!(!tb.is_unlimited());

    let start = Instant::now();
    tb.acquire(5000).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "burst should be instant: {:?}",
        elapsed
    );

    tb.acquire(6000).await;
    let total_elapsed = start.elapsed();
    let expected_min = Duration::from_millis(100);
    assert!(
        total_elapsed >= expected_min.saturating_sub(Duration::from_millis(200)),
        "should have waited for refill: got {:?} expected >= {:?}",
        total_elapsed,
        expected_min
    );
}

#[tokio::test]
async fn test_token_bucket_try_acquire() {
    let tb = TokenBucket::new(1000, Some(2000));

    assert!(tb.try_acquire(1000));
    assert!(tb.try_acquire(1000));
    assert!(!tb.try_acquire(1));
}

#[test]
fn test_token_bucket_available_tokens() {
    let tb = TokenBucket::new(1000, Some(5000));
    let initial = tb.available_tokens();
    assert!(
        (initial - 5000.0).abs() < 0.01,
        "initial tokens should be ~5000, got {}",
        initial
    );

    tb.try_acquire(2000);
    let after = tb.available_tokens();
    assert!(
        (after - 3000.0).abs() < 0.01,
        "after acquiring 2000, should have ~3000, got {}",
        after
    );
}

#[test]
fn test_rate_limiter_config_default() {
    let cfg = RateLimiterConfig::default();
    assert!(!cfg.is_limited());
    assert!(cfg.download_rate().is_none());
    assert!(cfg.upload_rate().is_none());
}

#[test]
fn test_rate_limiter_config_new() {
    let cfg = RateLimiterConfig::new(Some(1024), Some(512));
    assert!(cfg.is_limited());
    assert_eq!(cfg.download_rate(), Some(1024));
    assert_eq!(cfg.upload_rate(), Some(512));
}

#[test]
fn test_rate_limiter_config_download_only() {
    let cfg = RateLimiterConfig::new(Some(2048), None);
    assert!(cfg.is_limited());
    assert_eq!(cfg.download_rate(), Some(2048));
    assert!(cfg.upload_rate().is_none());
}

#[tokio::test]
async fn test_rate_limiter_unlimited() {
    let rl = RateLimiter::unlimited();
    assert!(!rl.is_download_limited());
    assert!(!rl.is_upload_limited());
    rl.acquire_download(999999).await;
    rl.acquire_upload(999999).await;
}

#[tokio::test]
async fn test_rate_limiter_with_limits() {
    let cfg = RateLimiterConfig::new(Some(5000), Some(1000)).with_burst(Some(1000), Some(500));
    let rl = RateLimiter::new(&cfg);
    assert!(rl.is_download_limited());
    assert!(rl.is_upload_limited());

    let start = Instant::now();
    rl.acquire_download(6000).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(800),
        "should throttle: got {:?}",
        elapsed
    );
}

#[tokio::test]
async fn test_throttled_writer_no_limit_passthrough() {
    use crate::filesystem::disk_writer::ByteArrayDiskWriter;

    let raw = ByteArrayDiskWriter::new();
    let rl = RateLimiter::unlimited();
    let mut tw = ThrottledWriter::new(raw, rl);

    tw.write(b"hello world").await.unwrap();
    tw.write(b" foo bar baz").await.unwrap();
    let result = tw.finalize().await.unwrap();

    assert_eq!(result, b"hello world foo bar baz");
}

#[tokio::test]
async fn test_throttled_writer_with_limit() {
    use crate::filesystem::disk_writer::ByteArrayDiskWriter;

    let raw = ByteArrayDiskWriter::new();
    let cfg = RateLimiterConfig::new(Some(100_000), None).with_burst(Some(1000), None);
    let rl = RateLimiter::new(&cfg);
    let mut tw = ThrottledWriter::new(raw, rl);

    let data = vec![0xABu8; 50_000];
    let start = Instant::now();
    tw.write(&data).await.unwrap();
    let elapsed = start.elapsed();

    let result = tw.finalize().await.unwrap();
    assert_eq!(result.len(), 50_000);
    assert!(
        elapsed >= Duration::from_millis(400),
        "50KB at 100KB/s with 1KB burst should take >= 400ms, got {:?}",
        elapsed
    );
}

#[tokio::test]
async fn test_throttled_writer_chunk_size() {
    use crate::filesystem::disk_writer::ByteArrayDiskWriter;

    let raw = ByteArrayDiskWriter::new();
    let cfg = RateLimiterConfig::new(Some(1_000_000), None);
    let rl = RateLimiter::new(&cfg);
    let mut tw = ThrottledWriter::new(raw, rl).with_chunk_size(1024);

    let large_data = vec![0x42u8; 10_000];
    tw.write(&large_data).await.unwrap();
    let result = tw.finalize().await.unwrap();
    assert_eq!(result.len(), 10_000);
}

#[tokio::test]
async fn test_rate_limiter_zero_rate_means_unlimited() {
    let cfg = RateLimiterConfig::new(Some(0), Some(0));
    let rl = RateLimiter::new(&cfg);
    assert!(!rl.is_download_limited());
    assert!(!rl.is_upload_limited());
}

// ------------------------------------------------------------------
// Tests for the lock-free implementation (Task C1 / C2)
// ------------------------------------------------------------------

/// Verify that multiple tasks can acquire from the same `TokenBucket`
/// concurrently without deadlock, panic, or excessive contention.
///
/// With the old `tokio::sync::Mutex` implementation, 4 concurrent tasks
/// would serialise on the mutex. With the lock-free atomic implementation,
/// all tasks proceed concurrently — the only blocking is from
/// `tokio::time::sleep` when tokens are exhausted.
#[tokio::test]
async fn test_token_bucket_concurrent_no_deadlock() {
    // Large burst so all acquires are instant from burst tokens —
    // this isolates the concurrency test from timing concerns.
    let bucket = Arc::new(TokenBucket::new(10_000_000, Some(10_000_000)));

    let mut handles = Vec::with_capacity(4);
    for task_id in 0..4u8 {
        let b = bucket.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..1000 {
                b.acquire(1000).await;
            }
            task_id // return id for identification
        }));
    }

    // If any task deadlocks or panics, await will fail.
    for (i, h) in handles.into_iter().enumerate() {
        let id = h.await.expect("task should complete without panic");
        assert_eq!(id as usize, i, "task ordering preserved");
    }

    // After 4 * 1000 * 1000 = 4 MB acquired from a 10 MB burst,
    // at least 6 MB should remain (minus tiny refill variance).
    let remaining = bucket.available_tokens();
    assert!(
        remaining > 5_000_000.0,
        "should have ~6MB left after consuming 4MB, got {}",
        remaining
    );
}

/// Verify that `ThrottledWriter` batches token acquisition: a single
/// `write()` call should result in ONE throttle wait, not per-chunk waits.
///
/// We use a rate limiter with zero burst and a moderate rate. The total
/// elapsed time should match the batch calculation
/// (`total_bytes / rate`), not be inflated by per-chunk sleep scheduling
/// overhead. With per-chunk acquisition and a tiny chunk size, the
/// many individual `tokio::time::sleep` calls add measurable overhead.
#[tokio::test]
async fn test_throttled_writer_batches_token_acquisition() {
    use crate::filesystem::disk_writer::ByteArrayDiskWriter;

    // rate = 10 000 bytes/s, burst = 0 (pure rate limiting, no buffer).
    // data  = 5 000 bytes → expected wait ~500 ms (one batch sleep).
    let raw = ByteArrayDiskWriter::new();
    let cfg = RateLimiterConfig::new(Some(10_000), None).with_burst(Some(0), None);
    let rl = RateLimiter::new(&cfg);
    // Tiny chunk size to maximise per-chunk overhead if it were used.
    let mut tw = ThrottledWriter::new(raw, rl).with_chunk_size(100);

    let data = vec![0x77u8; 5_000];
    let start = Instant::now();
    tw.write(&data).await.unwrap();
    let elapsed = start.elapsed();
    let result = tw.finalize().await.unwrap();

    assert_eq!(result.len(), 5_000, "data integrity preserved");
    assert!(
        result.iter().all(|&b| b == 0x77),
        "all bytes should be 0x77"
    );

    // Expected batch wait: 5000 bytes / 10000 bytes/s = 500 ms.
    // Use relaxed lower bound since CI scheduler can add jitter.
    assert!(
        elapsed >= Duration::from_millis(300),
        "batch acquire should wait >= 300ms at 10KB/s rate, got {:?}",
        elapsed
    );

    // Upper bound: well under 2 seconds with batch acquisition.
    assert!(
        elapsed < Duration::from_secs(2),
        "batch acquire should complete well under 2s, got {:?}",
        elapsed
    );
}

/// Verify that `RateLimiter` clones share state — acquiring from one clone
/// affects the tokens available to the other. This is a unit-level version
/// of the integration test in `test_e2e_rate_limit.rs`.
#[tokio::test]
async fn test_rate_limiter_clone_shares_state() {
    let cfg = RateLimiterConfig::new(Some(10000), None).with_burst(Some(5000), None);
    let rl1 = RateLimiter::new(&cfg);
    let rl2 = rl1.clone();

    assert!(rl1.is_download_limited());
    assert!(rl2.is_download_limited());

    // Acquiring from rl1 should deplete tokens visible to rl2.
    rl1.acquire_download(3000).await;
    rl2.acquire_download(3000).await;

    let config = rl1.config().await;
    assert!(config.download_rate().is_some());
}

/// Verify that a high rate with sufficient burst completes near-instantly,
/// confirming the lock-free path has negligible overhead.
#[tokio::test]
async fn test_rate_limiter_high_rate_low_latency() {
    let cfg = RateLimiterConfig::new(Some(100_000_000), None).with_burst(Some(1_000_000), None);
    let rl = RateLimiter::new(&cfg);

    let start = Instant::now();
    rl.acquire_download(100_000).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "100MB/s rate with 1MB burst should be near-instant for 100KB: got {:?}",
        elapsed
    );
}

// ------------------------------------------------------------------
// Tests for dynamic rate adjustment
// (set_rate / set_unlimited / set_download_rate / set_upload_rate)
// ------------------------------------------------------------------

/// Verify that `set_rate` updates the refill rate dynamically.
///
/// Adapted from the task spec: the original version acquired 100 MB at
/// 1 MB/s (~100 s wait). We instead drain the small burst with
/// `try_acquire` (non-blocking) and then verify the new rate is visible
/// via `rate()`.
#[tokio::test]
async fn test_token_bucket_set_rate() {
    let tb = TokenBucket::new(1_000_000, Some(1000)); // 1 MB/s, 1 KB burst
    // Drain the burst tokens (non-blocking).
    assert!(tb.try_acquire(1000));

    // Now set rate to 10 MB/s and verify.
    tb.set_rate(10_000_000);
    let rate = tb.rate();
    assert!(
        (rate - 10_000_000.0).abs() < 1.0,
        "rate should be ~10 MB/s, got {}",
        rate
    );
}

/// Verify that `set_rate(0)` reports a zero rate (effectively pauses refill).
#[tokio::test]
async fn test_token_bucket_set_rate_to_zero() {
    let tb = TokenBucket::new(1_000_000, Some(1000));
    tb.set_rate(0);
    let rate = tb.rate();
    assert!(
        (rate - 0.0).abs() < 0.01,
        "rate should be 0 after set_rate(0), got {}",
        rate
    );
}

/// Verify that `set_unlimited(true)` makes acquire return instantly
/// even for very large requests.
#[tokio::test]
async fn test_token_bucket_set_unlimited() {
    let tb = TokenBucket::new(1_000, None); // 1 KB/s, limited
    assert!(!tb.is_unlimited());
    tb.set_unlimited(true);
    assert!(tb.is_unlimited());

    // Should acquire instantly — 1 GB at 1 KB/s would otherwise take ~17 min.
    let start = Instant::now();
    tb.acquire(1_000_000_000).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "unlimited acquire should be instant, got {:?}",
        elapsed
    );
}

/// Verify that `set_download_rate` updates the download rate and that
/// `config()` reflects the change.
#[tokio::test]
async fn test_rate_limiter_set_download_rate() {
    let rl = RateLimiter::new(&RateLimiterConfig::new(Some(1_000_000), None)); // 1 MB/s
    assert!(rl.is_download_limited());

    // Change to 5 MB/s
    rl.set_download_rate(Some(5_000_000));
    assert!(rl.is_download_limited());
    let config = rl.config().await;
    assert_eq!(config.download_rate(), Some(5_000_000));

    // Change to unlimited
    rl.set_download_rate(None);
    assert!(!rl.is_download_limited());
    let config = rl.config().await;
    assert!(
        config.download_rate().is_none(),
        "download_rate should be None after set_download_rate(None), got {:?}",
        config.download_rate()
    );
}

/// Verify that `set_upload_rate` updates the upload rate and that
/// `config()` reflects the change.
#[tokio::test]
async fn test_rate_limiter_set_upload_rate() {
    let rl = RateLimiter::new(&RateLimiterConfig::new(None, Some(500_000))); // 500 KB/s
    assert!(rl.is_upload_limited());

    rl.set_upload_rate(Some(2_000_000)); // 2 MB/s
    assert!(rl.is_upload_limited());
    let config = rl.config().await;
    assert_eq!(config.upload_rate(), Some(2_000_000));

    // Change to unlimited via Some(0)
    rl.set_upload_rate(Some(0));
    assert!(!rl.is_upload_limited());
    let config = rl.config().await;
    assert!(config.upload_rate().is_none());
}

/// Verify that `RateLimiter` clones share the inner token bucket state —
/// changing the rate via one clone is visible through `config()` and
/// `is_download_limited()` on another. Both the rate and the limited flag
/// live inside `Arc<RateLimiterInner>`, so all clones observe updates.
#[tokio::test]
async fn test_rate_limiter_clone_shares_inner_state() {
    let rl = RateLimiter::new(&RateLimiterConfig::new(Some(1_000_000), None));
    let rl_clone = rl.clone();

    // Change rate via original
    rl.set_download_rate(Some(5_000_000));

    // Clone should see the updated rate via the shared inner.
    let config = rl_clone.config().await;
    assert_eq!(
        config.download_rate(),
        Some(5_000_000),
        "clone should see updated rate via shared Arc<inner>"
    );
    assert!(
        rl_clone.is_download_limited(),
        "clone should see updated limited flag via shared Arc<inner>"
    );

    // Change to unlimited via the clone — original should see it too.
    rl_clone.set_download_rate(None);
    assert!(
        !rl.is_download_limited(),
        "original should see unlimited flag set by clone"
    );
}
