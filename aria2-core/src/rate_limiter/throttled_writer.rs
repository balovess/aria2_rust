use async_trait::async_trait;

use crate::constants;
use crate::error::Result;
use crate::filesystem::disk_writer::{DiskWriter, SeekableDiskWriter};

use super::rate_limiter::RateLimiter;

/// A `DiskWriter` wrapper that throttles writes via a `RateLimiter`.
///
/// Token acquisition is **batched**: a single `write()` call acquires tokens
/// for the entire buffer upfront (one CAS sequence), then writes the data in
/// chunks to the inner writer. This avoids per-chunk lock contention — at
/// 1 GiB/s with 8 KB chunks that is 125 000 fewer acquire calls per second.
///
/// In addition to the per-download `limiter`, an optional `global_limiter`
/// can be set via [`with_global_limiter`](Self::with_global_limiter). When
/// present, tokens are acquired from the global limiter **after** the
/// per-download limiter, enforcing a process-wide bandwidth ceiling across
/// all concurrent downloads. Both limiters share the same `Arc`-backed
/// inner state, so cloning is cheap and all clones see the same rate.
pub struct ThrottledWriter<W> {
    inner: W,
    limiter: RateLimiter,
    /// Optional process-wide rate limiter (from `DownloadEngine::global_limiter`).
    /// When `Some` and `is_download_limited()`, tokens are acquired after the
    /// per-download limiter on every chunk write.
    global_limiter: Option<RateLimiter>,
    chunk_size: usize,
}

impl<W> ThrottledWriter<W>
where
    W: Send,
{
    pub fn new(inner: W, limiter: RateLimiter) -> Self {
        Self {
            inner,
            limiter,
            global_limiter: None,
            chunk_size: constants::RATE_LIMITER_CHUNK_SIZE,
        }
    }

    /// Attach a process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, every `write()` / `write_at()` acquires tokens from the
    /// global limiter **after** the per-download limiter. If the global
    /// limiter is `None` or `!is_download_limited()`, the global acquire
    /// is skipped (no overhead).
    pub fn with_global_limiter(mut self, limiter: RateLimiter) -> Self {
        self.global_limiter = Some(limiter);
        self
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
        let per_limited = self.limiter.is_download_limited();
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());

        if !per_limited && !global_limited {
            return self.inner.write(data).await;
        }

        // Acquire tokens per-chunk (not batched for the entire buffer).
        //
        // Rationale: reqwest's `bytes_stream()` yields chunks whose sizes grow
        // adaptively (8K -> 16K -> 32K -> ... -> 256K+) on fast links. Keeping
        // acquisition and writes interleaved prevents one large transport
        // chunk from delaying all disk I/O behind a single long wait. The
        // TokenBucket also listens for rate changes, so `changeOption` wakes a
        // pending acquire and the next chunk uses the new rate immediately.
        // The lock-free CAS in `TokenBucket::acquire` keeps this accounting
        // overhead low even at high rates.
        if data.len() <= self.chunk_size {
            if per_limited {
                self.limiter.acquire_download(data.len() as u64).await;
            }
            if global_limited {
                self.global_limiter
                    .as_ref()
                    .unwrap()
                    .acquire_download(data.len() as u64)
                    .await;
            }
            return self.inner.write(data).await;
        }

        let mut offset = 0usize;
        while offset < data.len() {
            let end = (offset + self.chunk_size).min(data.len());
            let chunk = &data[offset..end];
            let chunk_len = chunk.len() as u64;
            if per_limited {
                self.limiter.acquire_download(chunk_len).await;
            }
            if global_limited {
                self.global_limiter
                    .as_ref()
                    .unwrap()
                    .acquire_download(chunk_len)
                    .await;
            }
            self.inner.write(chunk).await?;
            offset = end;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        self.inner.finalize().await
    }
}

#[async_trait]
impl<W> SeekableDiskWriter for ThrottledWriter<W>
where
    W: SeekableDiskWriter + Send,
{
    async fn open(&mut self) -> Result<()> {
        self.inner.open().await
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let per_limited = self.limiter.is_download_limited();
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());

        if !per_limited && !global_limited {
            return self.inner.write_at(offset, data).await;
        }

        if data.len() <= self.chunk_size {
            if per_limited {
                self.limiter.acquire_download(data.len() as u64).await;
            }
            if global_limited {
                self.global_limiter
                    .as_ref()
                    .unwrap()
                    .acquire_download(data.len() as u64)
                    .await;
            }
            return self.inner.write_at(offset, data).await;
        }

        let mut off = offset;
        let mut idx = 0usize;
        while idx < data.len() {
            let end = (idx + self.chunk_size).min(data.len());
            let chunk_len = (end - idx) as u64;
            if per_limited {
                self.limiter.acquire_download(chunk_len).await;
            }
            if global_limited {
                self.global_limiter
                    .as_ref()
                    .unwrap()
                    .acquire_download(chunk_len)
                    .await;
            }
            self.inner.write_at(off, &data[idx..end]).await?;
            off += chunk_len;
            idx = end;
        }
        Ok(())
    }

    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        let per_limited = self.limiter.is_download_limited();
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());

        if !per_limited && !global_limited {
            return self.inner.write_bytes_at(offset, data).await;
        }
        self.write_at(offset, &data).await
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.inner.read_at(offset, buf).await
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        self.inner.truncate(length).await
    }

    async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await
    }

    async fn len(&self) -> Result<u64> {
        self.inner.len().await
    }

    fn path(&self) -> &std::path::Path {
        self.inner.path()
    }

    async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }
}
