use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::error::Result;

const DEFAULT_BURST_BYTES: u64 = 256 * 1024;

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

pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(rate_bytes_per_sec: u64, burst_bytes: Option<u64>) -> Self {
        let burst = burst_bytes.unwrap_or(DEFAULT_BURST_BYTES) as f64;
        Self {
            capacity: burst,
            tokens: burst,
            rate: rate_bytes_per_sec as f64,
            last_refill: Instant::now(),
        }
    }

    pub fn unlimited() -> Self {
        Self {
            capacity: f64::MAX,
            tokens: f64::MAX,
            rate: f64::MAX,
            last_refill: Instant::now(),
        }
    }

    pub fn is_unlimited(&self) -> bool {
        self.rate >= f64::MAX
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
    }

    pub async fn acquire(&mut self, bytes: u64) {
        if self.is_unlimited() {
            return;
        }
        self.refill();

        let needed = bytes as f64;
        if needed <= self.tokens {
            self.tokens -= needed;
            return;
        }

        let deficit = needed - self.tokens;
        let wait_secs = deficit / self.rate;
        self.tokens = 0.0;
        self.last_refill = Instant::now();

        if wait_secs > 0.000001 {
            tokio::time::sleep(Duration::from_secs_f64(wait_secs)).await;
        }

        self.refill();
        self.tokens = (self.tokens - needed).max(0.0);
    }

    pub fn try_acquire(&mut self, bytes: u64) -> bool {
        if self.is_unlimited() {
            return true;
        }
        self.refill();
        let needed = bytes as f64;
        if needed <= self.tokens {
            self.tokens -= needed;
            true
        } else {
            false
        }
    }

    pub fn available_tokens(&self) -> f64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        (self.tokens + elapsed * self.rate).min(self.capacity)
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }
}

#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<RateLimiterInner>>,
    download_limited: bool,
    upload_limited: bool,
}

struct RateLimiterInner {
    download: TokenBucket,
    upload: TokenBucket,
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
            inner: Arc::new(Mutex::new(RateLimiterInner { download, upload })),
            download_limited: dl_rate.is_some_and(|r| r > 0),
            upload_limited: ul_rate.is_some_and(|r| r > 0),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(&RateLimiterConfig::default())
    }

    pub async fn acquire_download(&self, bytes: u64) {
        let mut inner = self.inner.lock().await;
        inner.download.acquire(bytes).await;
    }

    pub async fn acquire_upload(&self, bytes: u64) {
        let mut inner = self.inner.lock().await;
        inner.upload.acquire(bytes).await;
    }

    pub fn is_download_limited(&self) -> bool {
        self.download_limited
    }

    pub fn is_upload_limited(&self) -> bool {
        self.upload_limited
    }

    pub async fn config(&self) -> RateLimiterConfig {
        let inner = self.inner.lock().await;
        RateLimiterConfig::new(
            if inner.download.is_unlimited() {
                None
            } else {
                Some(inner.download.rate() as u64)
            },
            if inner.upload.is_unlimited() {
                None
            } else {
                Some(inner.upload.rate() as u64)
            },
        )
    }
}

pub struct ThrottledWriter<W> {
    inner: W,
    limiter: RateLimiter,
    #[allow(dead_code)] // Buffer for future write batching optimization
    buffer: Vec<u8>,
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
            buffer: Vec::with_capacity(64 * 1024),
            chunk_size: 8192,
        }
    }

    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(512);
        self
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }
}

use crate::filesystem::disk_writer::DiskWriter;

#[async_trait]
impl<W> DiskWriter for ThrottledWriter<W>
where
    W: DiskWriter + Send,
{
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        if !self.limiter.is_download_limited() {
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

    #[tokio::test]
    async fn test_token_bucket_unlimited() {
        let mut tb = TokenBucket::unlimited();
        assert!(tb.is_unlimited());
        tb.acquire(1024 * 1024 * 1024).await;
        assert!(tb.available_tokens() > 0.0);
    }

    #[tokio::test]
    async fn test_token_bucket_basic_acquire() {
        let mut tb = TokenBucket::new(10000, Some(5000));
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
        let mut tb = TokenBucket::new(1000, Some(2000));

        assert!(tb.try_acquire(1000));
        assert!(tb.try_acquire(1000));
        assert!(!tb.try_acquire(1));
    }

    #[test]
    fn test_token_bucket_available_tokens() {
        let mut tb = TokenBucket::new(1000, Some(5000));
        let initial = tb.available_tokens();
        assert!((initial - 5000.0).abs() < 0.01);

        tb.try_acquire(2000);
        let after = tb.available_tokens();
        assert!((after - 3000.0).abs() < 0.01);
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
}
