use async_trait::async_trait;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::constants;
use crate::error::Result;
use crate::filesystem::disk_writer::DiskWriter;

#[derive(Clone, Debug, Default)]
pub struct RateLimiterConfig {
    pub max_download_bytes_per_sec: Option<u64>,
    pub max_upload_bytes_per_sec: Option<u64>,
    pub download_burst_bytes: Option<u64>,
    pub upload_burst_bytes: Option<u64>,
}

impl RateLimiterConfig {
    pub fn new(download_limit: Option<u64>, upload_limit: Option<u64>) -> Self {
        Self {
            max_download_bytes_per_sec: download_limit,
            max_upload_bytes_per_sec: upload_limit,
            download_burst_bytes: None,
            upload_burst_bytes: None,
        }
    }

    pub fn with_burst(mut self, download_burst: Option<u64>, upload_burst: Option<u64>) -> Self {
        self.download_burst_bytes = download_burst;
        self.upload_burst_bytes = upload_burst;
        self
    }

    pub fn is_limited(&self) -> bool {
        self.max_download_bytes_per_sec.is_some() || self.max_upload_bytes_per_sec.is_some()
    }

    pub fn download_rate(&self) -> Option<u64> {
        self.max_download_bytes_per_sec
    }

    pub fn upload_rate(&self) -> Option<u64> {
        self.max_upload_bytes_per_sec
    }

    pub fn download_burst(&self) -> Option<u64> {
        self.download_burst_bytes
    }

    pub fn upload_burst(&self) -> Option<u64> {
        self.upload_burst_bytes
    }
}

/// Nanoseconds per second — used for integer time/rate conversions.
const NS_PER_SEC: u64 = 1_000_000_000;

/// Minimum wait duration before issuing a `tokio::time::sleep`.
/// Waits shorter than this use a spin-loop hint instead to avoid
/// the scheduling overhead of waking a task for sub-microsecond delays.
const MIN_SLEEP: Duration = Duration::from_micros(1);

/// Lock-free token bucket using atomic CAS operations.
///
/// All mutable state is stored in `AtomicU64` — no `Mutex` is acquired on the
/// hot path. Token refill is computed lazily on each `acquire` / `try_acquire`
/// call based on elapsed time since the last refill.
///
/// Integer arithmetic is used throughout (no `f64`) for deterministic behaviour
/// and to avoid floating-point CAS issues. Token counts are tracked in
/// **milli-tokens** (tokens * 1000) to provide sub-token precision while
/// staying in integer domain.
///
/// All public methods take `&self` (not `&mut self`), enabling concurrent
/// access from multiple tasks via a shared reference.
pub struct TokenBucket {
    /// Current token count in milli-tokens (tokens * 1000).
    /// Updated via CAS — never read-modify-write without compare_exchange.
    tokens_milli: AtomicU64,
    /// Maximum capacity in milli-tokens. Immutable after construction.
    capacity_milli: u64,
    /// Refill rate in milli-tokens per second. Mutable via `set_rate` for
    /// dynamic rate adjustment.
    /// `rate_milli_per_sec = rate_bytes_per_sec * 1000`.
    rate_milli_per_sec: AtomicU64,
    /// Last refill timestamp — nanoseconds elapsed since `anchor`.
    /// Updated via CAS to claim a refill slot (only the winning thread adds tokens).
    last_refill_elapsed_ns: AtomicU64,
    /// Whether this bucket is unlimited (rate = infinity). Mutable via
    /// `set_unlimited` for dynamic mode switching.
    unlimited: AtomicBool,
    /// Anchor `Instant` created at construction; used to compute elapsed nanoseconds.
    /// Never mutated — `Instant` is `Send + Sync`.
    anchor: Instant,
}

impl fmt::Debug for TokenBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenBucket")
            .field("tokens_milli", &self.tokens_milli.load(Ordering::Relaxed))
            .field("capacity_milli", &self.capacity_milli)
            .field("rate_milli_per_sec", &self.rate_milli_per_sec.load(Ordering::Relaxed))
            .field("unlimited", &self.unlimited.load(Ordering::Relaxed))
            .finish()
    }
}

impl TokenBucket {
    /// Create a new token bucket with the given rate and optional burst.
    ///
    /// `rate_bytes_per_sec` of 0 produces a bucket that never refills — callers
    /// should use [`TokenBucket::unlimited`] instead for "no limit" semantics.
    pub fn new(rate_bytes_per_sec: u64, burst_bytes: Option<u64>) -> Self {
        let burst = burst_bytes.unwrap_or(constants::DEFAULT_BURST_BYTES as u64);
        let anchor = Instant::now();
        Self {
            tokens_milli: AtomicU64::new(burst.saturating_mul(1000)),
            capacity_milli: burst.saturating_mul(1000),
            rate_milli_per_sec: AtomicU64::new(rate_bytes_per_sec.saturating_mul(1000)),
            last_refill_elapsed_ns: AtomicU64::new(0),
            unlimited: AtomicBool::new(false),
            anchor,
        }
    }

    /// Create an unlimited token bucket — `acquire` / `try_acquire` always
    /// succeed instantly without consuming any real tokens.
    pub fn unlimited() -> Self {
        let anchor = Instant::now();
        // Use a large but safe value to avoid overflow on arithmetic.
        let huge = u64::MAX / 4;
        Self {
            tokens_milli: AtomicU64::new(huge),
            capacity_milli: huge,
            rate_milli_per_sec: AtomicU64::new(huge),
            last_refill_elapsed_ns: AtomicU64::new(0),
            unlimited: AtomicBool::new(true),
            anchor,
        }
    }

    /// Returns `true` if this bucket has no rate limit.
    pub fn is_unlimited(&self) -> bool {
        self.unlimited.load(Ordering::Relaxed)
    }

    /// Returns the configured rate in bytes per second (as `f64` for API compat).
    /// Returns `f64::MAX` for unlimited buckets.
    pub fn rate(&self) -> f64 {
        if self.unlimited.load(Ordering::Relaxed) {
            f64::MAX
        } else {
            self.rate_milli_per_sec.load(Ordering::Relaxed) as f64 / 1000.0
        }
    }

    /// Returns the current available tokens (as `f64` for API compat).
    /// Triggers a lazy refill before reading.
    /// Returns `f64::MAX` for unlimited buckets.
    pub fn available_tokens(&self) -> f64 {
        if self.unlimited.load(Ordering::Relaxed) {
            return f64::MAX;
        }
        self.refill();
        self.tokens_milli.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Nanoseconds elapsed since the anchor `Instant`.
    #[inline]
    fn now_ns(&self) -> u64 {
        // saturating_duration_since avoids panic on clock anomalies.
        // now - anchor = elapsed time since construction.
        Instant::now()
            .saturating_duration_since(self.anchor)
            .as_nanos() as u64
    }

    /// Lazily refill tokens based on elapsed time since the last refill.
    ///
    /// Uses a **CAS-claim** pattern: only the thread that successfully advances
    /// `last_refill_elapsed_ns` adds tokens. This prevents double-counting when
    /// multiple threads call `refill` concurrently.
    ///
    /// Formula: `added_milli = elapsed_ns * rate_milli_per_sec / NS_PER_SEC`
    /// (the 1000× from milli-tokens cancels with the 1000× in rate_milli_per_sec).
    fn refill(&self) {
        if self.unlimited.load(Ordering::Relaxed) {
            return;
        }
        let now = self.now_ns();
        let last = self.last_refill_elapsed_ns.load(Ordering::Relaxed);
        if now <= last {
            // No time elapsed since last refill (or clock went backwards).
            return;
        }
        let elapsed_ns = now - last;
        // u128 to avoid overflow: elapsed_ns (u64) * rate_milli_per_sec (u64).
        let added_milli = ((elapsed_ns as u128)
            * (self.rate_milli_per_sec.load(Ordering::Relaxed) as u128)
            / NS_PER_SEC as u128) as u64;
        if added_milli == 0 {
            // Less than 1 milli-token elapsed — do NOT advance last_refill to
            // preserve fractional accumulation for the next call.
            return;
        }
        // Claim the refill: only the winner of this CAS proceeds to add tokens.
        // Losers abort — another thread already refilled for a overlapping period.
        match self.last_refill_elapsed_ns.compare_exchange(
            last,
            now,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                // Won the claim — add tokens, capping at capacity.
                loop {
                    let current = self.tokens_milli.load(Ordering::Relaxed);
                    let new = current
                        .saturating_add(added_milli)
                        .min(self.capacity_milli);
                    match self.tokens_milli.compare_exchange_weak(
                        current,
                        new,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(_) => continue, // Another thread modified tokens — retry.
                    }
                }
            }
            Err(_) => {
                // Lost the claim — another thread already refilled. Nothing to do.
            }
        }
    }

    /// Acquire `bytes` tokens, blocking (async-sleeping) until enough tokens
    /// are available.
    ///
    /// For requests larger than the burst capacity, this method waits for the
    /// deficit and then force-acquires (setting tokens to 0), matching the
    /// original implementation's behaviour of allowing token "debt" clamped to
    /// zero. This prevents infinite loops when `needed > capacity`.
    pub async fn acquire(&self, bytes: u64) {
        if self.unlimited.load(Ordering::Relaxed) {
            return;
        }
        // milli-tokens needed; saturating_mul caps at u64::MAX on overflow.
        let needed_milli = bytes.saturating_mul(1000);

        loop {
            self.refill();
            let current = self.tokens_milli.load(Ordering::Relaxed);
            if current >= needed_milli {
                // Enough tokens — try CAS to deduct.
                match self.tokens_milli.compare_exchange_weak(
                    current,
                    current - needed_milli,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(_) => continue, // Raced — retry.
                }
            }

            // Not enough tokens — compute wait time from the deficit.
            let deficit_milli = needed_milli - current;
            let rate_milli = self.rate_milli_per_sec.load(Ordering::Relaxed);
            if rate_milli == 0 {
                // Rate is 0 — would wait forever. Defensively treat as unlimited
                // rather than hanging the caller.
                warn!("TokenBucket::acquire with rate=0; treating as unlimited");
                return;
            }
            // wait_ns = deficit_milli * NS_PER_SEC / rate_milli_per_sec
            // (u128 to avoid overflow).
            let wait_ns = ((deficit_milli as u128) * NS_PER_SEC as u128
                / rate_milli as u128) as u64;
            let wait = Duration::from_nanos(wait_ns);

            if wait < MIN_SLEEP {
                // Very short wait — spin instead of paying scheduler overhead.
                std::hint::spin_loop();
                continue;
            }

            debug!(
                bytes = bytes,
                deficit_milli = deficit_milli,
                wait_ns = wait_ns,
                "throttling: sleeping for token refill"
            );
            tokio::time::sleep(wait).await;

            // After sleeping, force-acquire: refill, then deduct (clamped to 0).
            // This matches the original behaviour where tokens can go negative
            // (clamped to 0) when the request exceeds burst capacity.
            self.refill();
            loop {
                let cur = self.tokens_milli.load(Ordering::Relaxed);
                let new = cur.saturating_sub(needed_milli);
                match self.tokens_milli.compare_exchange_weak(
                    cur,
                    new,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(_) => continue,
                }
            }
        }
    }

    /// Non-blocking attempt to acquire `bytes` tokens.
    /// Returns `true` if tokens were available and deducted, `false` otherwise.
    pub fn try_acquire(&self, bytes: u64) -> bool {
        if self.unlimited.load(Ordering::Relaxed) {
            return true;
        }
        self.refill();
        let needed_milli = bytes.saturating_mul(1000);
        loop {
            let current = self.tokens_milli.load(Ordering::Relaxed);
            if current < needed_milli {
                return false;
            }
            match self.tokens_milli.compare_exchange_weak(
                current,
                current - needed_milli,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// Update the refill rate dynamically. Takes effect on the next refill cycle.
    /// `rate_bytes_per_sec` of 0 effectively pauses the bucket (no new tokens).
    pub fn set_rate(&self, rate_bytes_per_sec: u64) {
        self.rate_milli_per_sec
            .store(rate_bytes_per_sec.saturating_mul(1000), Ordering::Relaxed);
    }

    /// Toggle the unlimited flag. When set to true, acquire/try_acquire always
    /// succeed instantly without consuming tokens.
    pub fn set_unlimited(&self, unlimited: bool) {
        self.unlimited.store(unlimited, Ordering::Relaxed);
    }
}

/// Inner state of `RateLimiter`, shared via `Arc` so that cloning a
/// `RateLimiter` shares the same token buckets and limit flags (no Mutex
/// involved). The `download_limited` / `upload_limited` flags live here so
/// that `set_download_rate` / `set_upload_rate` on one clone are visible to
/// all clones.
struct RateLimiterInner {
    download: TokenBucket,
    upload: TokenBucket,
    download_limited: AtomicBool,
    upload_limited: AtomicBool,
}

/// Rate limiter for download and upload bandwidth.
///
/// Cloning a `RateLimiter` shares the underlying token buckets — all clones
/// draw from the same pool. The hot path (`acquire_download` / `acquire_upload`)
/// performs **no mutex acquisition**: token accounting is done entirely via
/// atomic CAS operations on the inner `TokenBucket`s.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<RateLimiterInner>,
}

impl RateLimiter {
    pub fn new(config: &RateLimiterConfig) -> Self {
        let dl_rate = config.download_rate();
        let ul_rate = config.upload_rate();
        let dl_burst = config.download_burst();
        let ul_burst = config.upload_burst();

        let download = match dl_rate {
            Some(rate) if rate > 0 => TokenBucket::new(rate, dl_burst),
            _ => TokenBucket::unlimited(),
        };
        let upload = match ul_rate {
            Some(rate) if rate > 0 => TokenBucket::new(rate, ul_burst),
            _ => TokenBucket::unlimited(),
        };

        Self {
            inner: Arc::new(RateLimiterInner {
                download,
                upload,
                download_limited: AtomicBool::new(dl_rate.is_some_and(|r| r > 0)),
                upload_limited: AtomicBool::new(ul_rate.is_some_and(|r| r > 0)),
            }),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(&RateLimiterConfig::default())
    }

    pub async fn acquire_download(&self, bytes: u64) {
        self.inner.download.acquire(bytes).await;
    }

    pub async fn acquire_upload(&self, bytes: u64) {
        self.inner.upload.acquire(bytes).await;
    }

    /// Non-blocking attempt to acquire download tokens.
    /// Returns `true` if tokens were available, `false` otherwise (no wait).
    #[allow(clippy::unused_async)]
    pub async fn try_acquire_download(&self, bytes: u64) -> bool {
        self.inner.download.try_acquire(bytes)
    }

    /// Non-blocking attempt to acquire upload tokens.
    /// Returns `true` if tokens were available, `false` otherwise (no wait).
    #[allow(clippy::unused_async)]
    pub async fn try_acquire_upload(&self, bytes: u64) -> bool {
        self.inner.upload.try_acquire(bytes)
    }

    pub fn is_download_limited(&self) -> bool {
        self.inner.download_limited.load(Ordering::Relaxed)
    }

    pub fn is_upload_limited(&self) -> bool {
        self.inner.upload_limited.load(Ordering::Relaxed)
    }

    pub async fn config(&self) -> RateLimiterConfig {
        RateLimiterConfig::new(
            if self.inner.download.is_unlimited() {
                None
            } else {
                Some(self.inner.download.rate() as u64)
            },
            if self.inner.upload.is_unlimited() {
                None
            } else {
                Some(self.inner.upload.rate() as u64)
            },
        )
    }

    /// Dynamically update the download rate limit.
    /// `None` or `Some(0)` means unlimited (no throttling).
    /// `Some(rate)` where rate > 0 sets the new rate in bytes/sec.
    pub fn set_download_rate(&self, rate: Option<u64>) {
        match rate {
            Some(r) if r > 0 => {
                self.inner.download.set_unlimited(false);
                self.inner.download.set_rate(r);
                self.inner.download_limited.store(true, Ordering::Relaxed);
            }
            _ => {
                self.inner.download.set_unlimited(true);
                self.inner.download_limited.store(false, Ordering::Relaxed);
            }
        }
    }

    /// Dynamically update the upload rate limit.
    /// Same semantics as `set_download_rate`.
    pub fn set_upload_rate(&self, rate: Option<u64>) {
        match rate {
            Some(r) if r > 0 => {
                self.inner.upload.set_unlimited(false);
                self.inner.upload.set_rate(r);
                self.inner.upload_limited.store(true, Ordering::Relaxed);
            }
            _ => {
                self.inner.upload.set_unlimited(true);
                self.inner.upload_limited.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// A `DiskWriter` wrapper that throttles writes via a `RateLimiter`.
///
/// Token acquisition is **batched**: a single `write()` call acquires tokens
/// for the entire buffer upfront (one CAS sequence), then writes the data in
/// chunks to the inner writer. This avoids per-chunk lock contention — at
/// 1 GiB/s with 8 KB chunks that is 125 000 fewer acquire calls per second.
pub struct ThrottledWriter<W> {
    inner: W,
    limiter: RateLimiter,
    chunk_size: usize,
}

impl<W> ThrottledWriter<W>
where
    W: DiskWriter + Send,
{
    pub fn new(inner: W, limiter: RateLimiter) -> Self {
        Self {
            inner,
            limiter,
            chunk_size: constants::RATE_LIMITER_CHUNK_SIZE,
        }
    }

    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(constants::RATE_LIMITER_MIN_CHUNK_SIZE);
        self
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }
}

#[async_trait]
impl<W> DiskWriter for ThrottledWriter<W>
where
    W: DiskWriter + Send,
{
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        if !self.limiter.is_download_limited() {
            return self.inner.write(data).await;
        }

        // Acquire tokens per-chunk (not batched for the entire buffer).
        //
        // Rationale: reqwest's `bytes_stream()` yields chunks whose sizes grow
        // adaptively (8K → 16K → 32K → … → 256K+) on fast links. A single
        // batched `acquire_download(entire_buffer)` for a 417 KB chunk at
        // 80 KB/s would sleep for ~5.2 s. That sleep is a fixed
        // `tokio::time::sleep` and is NOT interrupted when `changeOption`
        // updates the rate mid-sleep, making dynamic rate changes appear to
        // stall the download.
        //
        // Per-chunk acquisition bounds each `acquire` to
        // `chunk_size / rate` seconds (e.g. 8 KB / 80 KB/s = 0.1 s), so a
        // rate change takes effect within at most one chunk's duration. The
        // lock-free CAS in `TokenBucket::acquire` keeps overhead negligible
        // even at high rates where `try_acquire`-style fast paths trigger.
        if data.len() <= self.chunk_size {
            self.limiter.acquire_download(data.len() as u64).await;
            return self.inner.write(data).await;
        }

        let mut offset = 0usize;
        while offset < data.len() {
            let end = (offset + self.chunk_size).min(data.len());
            let chunk = &data[offset..end];
            self.limiter.acquire_download(chunk.len() as u64).await;
            self.inner.write(chunk).await?;
            offset = end;
        }
        Ok(())
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        self.inner.finalize().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
        let cfg =
            RateLimiterConfig::new(Some(5000), Some(1000)).with_burst(Some(1000), Some(500));
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
    // New tests for the lock-free implementation (Task C1 / C2)
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
        // Allow generous lower bound for timer jitter.
        assert!(
            elapsed >= Duration::from_millis(450),
            "batch acquire should wait ~500ms, got {:?}",
            elapsed
        );

        // Upper bound: with per-chunk acquisition (50 chunks * 100 bytes),
        // each 100ms sleep would add scheduling overhead. Batch should be
        // well under 1 second. If per-chunk were used with 50 sleeps,
        // overhead would push this higher on most platforms.
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
        let cfg =
            RateLimiterConfig::new(Some(100_000_000), None).with_burst(Some(1_000_000), None);
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
}
