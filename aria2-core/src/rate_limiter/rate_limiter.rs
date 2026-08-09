use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::config::RateLimiterConfig;
use super::token_bucket::TokenBucket;

/// Inner state of `RateLimiter`, shared via `Arc` so that cloning a
/// `RateLimiter` shares the same token buckets and limit flags (no Mutex
/// involved). The `download_limited` / `upload_limited` flags live here so
/// that `set_download_rate` / `set_upload_rate` on one clone are visible to
/// all clones.
pub(super) struct RateLimiterInner {
    pub(super) download: TokenBucket,
    pub(super) upload: TokenBucket,
    pub(super) download_limited: AtomicBool,
    pub(super) upload_limited: AtomicBool,
}

/// Rate limiter for download and upload bandwidth.
///
/// Cloning a `RateLimiter` shares the underlying token buckets — all clones
/// draw from the same pool. The hot path (`acquire_download` / `acquire_upload`)
/// performs **no mutex acquisition**: token accounting is done entirely via
/// atomic CAS operations on the inner `TokenBucket`s.
#[derive(Clone)]
pub struct RateLimiter {
    pub(super) inner: Arc<RateLimiterInner>,
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
