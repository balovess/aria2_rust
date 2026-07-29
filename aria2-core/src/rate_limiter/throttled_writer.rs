use async_trait::async_trait;

use crate::constants;
use crate::error::Result;
use crate::filesystem::disk_writer::DiskWriter;

use super::rate_limiter::RateLimiter;

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
