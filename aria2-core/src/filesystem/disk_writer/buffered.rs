//! Buffered (cached) disk writer.
//!
//! [`CachedDiskWriter`] layers a write-back cache over a positioned I/O
//! strategy ([`PositionedDiskWriter`] or [`MmapDiskWriter`]), providing
//! rate-limiting and direct-write bypass for large payloads.

use crate::error::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

use super::super::disk_cache::{DiskCacheStats, WrDiskCache};
use super::super::mmap_disk_writer::MmapDiskWriter;
use super::super::positioned_disk_writer::PositionedDiskWriter;
use super::SeekableDiskWriter;

/// Fixed threshold: writes >= 1MB bypass the cache and go directly to disk.
const DIRECT_WRITE_THRESHOLD: usize = 1024 * 1024;

/// Write counters for one [`CachedDiskWriter`] instance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachedDiskWriterStats {
    pub cache: DiskCacheStats,
    pub direct_write_count: u64,
    pub direct_write_bytes: u64,
}

pub struct CachedDiskWriter {
    /// The underlying positioned/mmap writer. Held as a trait object so the
    /// concrete strategy (PositionedDiskWriter vs MmapDiskWriter) can be
    /// selected at construction time. Unlike the legacy `Arc<Mutex<>>` design,
    /// there is NO internal async mutex - writes go directly to the writer,
    /// eliminating lock contention across `.await` points.
    writer: Box<dyn SeekableDiskWriter>,
    cache: Option<Arc<WrDiskCache>>,
    path: PathBuf,
    total_size: Option<u64>,
    opened: bool,
    // Rate limiter for write throttling
    rate_limiter: Option<Arc<crate::rate_limiter::RateLimiter>>,
    direct_write_count: u64,
    direct_write_bytes: u64,
}

impl CachedDiskWriter {
    /// Create a new `CachedDiskWriter` using [`PositionedDiskWriter`] (pwrite/seek_write)
    /// as the underlying I/O strategy.
    ///
    /// This is the default constructor; use [`new_with_mmap`](Self::new_with_mmap)
    /// to select the memory-mapped strategy instead.
    pub fn new(path: &Path, total_size: Option<u64>, cache_size_mb: Option<usize>) -> Self {
        Self::new_with_mmap(path, total_size, cache_size_mb, false)
    }

    /// Create a new `CachedDiskWriter` with explicit control over the I/O strategy.
    ///
    /// # Arguments
    /// * `path` - Output file path.
    /// * `total_size` - Expected total file size, used for pre-allocation.
    /// * `cache_size_mb` - Optional write-back cache size in megabytes.
    /// * `use_mmap` - If `true`, use [`MmapDiskWriter`] (memory-mapped I/O);
    ///   otherwise use [`PositionedDiskWriter`] (positioned `pwrite`/`seek_write`).
    pub fn new_with_mmap(
        path: &Path,
        total_size: Option<u64>,
        cache_size_mb: Option<usize>,
        use_mmap: bool,
    ) -> Self {
        Self::new_with_mmap_bytes(
            path,
            total_size,
            cache_size_mb.map(|mb| mb as u64 * 1024 * 1024),
            use_mmap,
        )
    }

    /// Create a writer with an exact write-back cache capacity in bytes.
    pub fn new_with_mmap_bytes(
        path: &Path,
        total_size: Option<u64>,
        cache_size_bytes: Option<u64>,
        use_mmap: bool,
    ) -> Self {
        let writer: Box<dyn SeekableDiskWriter> = if use_mmap {
            Box::new(MmapDiskWriter::new(path, total_size))
        } else {
            Box::new(PositionedDiskWriter::new(path, total_size))
        };
        let cache = cache_size_bytes
            .filter(|size| *size > 0)
            .and_then(|size| usize::try_from(size).ok())
            .map(|size| Arc::new(WrDiskCache::with_max_size_bytes(size)));
        Self {
            writer,
            cache,
            path: path.to_path_buf(),
            total_size,
            opened: false,
            rate_limiter: None,
            direct_write_count: 0,
            direct_write_bytes: 0,
        }
    }

    pub fn open_existing(path: &Path) -> Result<Self> {
        let mut writer = Self::new(path, None, None);
        writer.opened = true;
        Ok(writer)
    }

    /// Attach a rate limiter for write throttling.
    /// Uses non-blocking try_acquire: if tokens unavailable, writes proceed without blocking.
    pub fn with_rate_limiter(mut self, limiter: Arc<crate::rate_limiter::RateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    pub fn is_opened(&self) -> bool {
        self.opened
    }

    /// Return cache and direct-write counters for this writer.
    pub fn stats(&self) -> CachedDiskWriterStats {
        CachedDiskWriterStats {
            cache: self
                .cache
                .as_ref()
                .map_or_else(DiskCacheStats::default, |cache| cache.stats()),
            direct_write_count: self.direct_write_count,
            direct_write_bytes: self.direct_write_bytes,
        }
    }

    fn record_direct_write(&mut self, bytes: usize) {
        self.direct_write_count += 1;
        self.direct_write_bytes += bytes as u64;
    }
}

#[async_trait]
impl SeekableDiskWriter for CachedDiskWriter {
    async fn open(&mut self) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        // Delegate to the underlying writer (PositionedDiskWriter or
        // MmapDiskWriter). Both handle parent-dir creation, file creation,
        // and pre-allocation internally - no external logic needed here.
        self.writer.open().await?;
        self.opened = true;
        Ok(())
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.open().await?;

        // Rate limiting - non-blocking try_acquire
        if let Some(ref limiter) = self.rate_limiter
            && !limiter.try_acquire_download(data.len() as u64).await
        {
            debug!(
                "Rate limit exceeded for {} bytes at offset {}, writing without throttling",
                data.len(),
                offset
            );
        }

        if data.len() >= DIRECT_WRITE_THRESHOLD {
            // Drain older cached ranges first. Otherwise a later flush could
            // overwrite this newer direct write with stale bytes.
            self.flush_cache().await?;
            self.writer.write_at(offset, data).await?;
            self.record_direct_write(data.len());
        } else if let Some(ref cache) = self.cache {
            // Small writes go to the write-back cache.
            // copy_from_slice is unavoidable here: we only have a &[u8],
            // and the cache stores Bytes (Arc-backed).
            cache
                .write(offset, bytes::Bytes::copy_from_slice(data))
                .await?;
        } else {
            // No cache configured - write directly.
            self.writer.write_at(offset, data).await?;
            self.record_direct_write(data.len());
        }

        Ok(())
    }

    /// Zero-copy write: accepts Bytes directly. When caching, the Bytes is
    /// moved into the cache (O(1) refcount bump). When direct-writing, the
    /// Bytes is passed by reference to pwrite (no copy).
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        self.open().await?;
        let data_len = data.len();

        // Rate limiting - non-blocking try_acquire
        if let Some(ref limiter) = self.rate_limiter
            && !limiter.try_acquire_download(data.len() as u64).await
        {
            debug!(
                "Rate limit exceeded for {} bytes at offset {}, writing without throttling",
                data.len(),
                offset
            );
        }

        if data.len() >= DIRECT_WRITE_THRESHOLD {
            // Drain older cached ranges first. Otherwise a later flush could
            // overwrite this newer direct write with stale bytes.
            self.flush_cache().await?;
            self.writer.write_bytes_at(offset, data).await?;
            self.record_direct_write(data_len);
        } else if let Some(ref cache) = self.cache {
            // Small writes go to the cache - zero-copy (move Bytes).
            cache.write(offset, data).await?;
        } else {
            // No cache configured - zero-copy to pwrite.
            self.writer.write_bytes_at(offset, data).await?;
            self.record_direct_write(data_len);
        }

        Ok(())
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        // Flush any cached dirty entries before reading so the read sees
        // the most recent writes.
        self.flush_cache().await?;
        // The underlying writer reads directly into buf - no intermediate
        // Vec allocation (unlike the legacy DirectDiskAdaptor::read).
        self.writer.read_at(offset, buf).await
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        self.flush_cache().await?;
        self.writer.truncate(length).await
    }

    async fn flush(&mut self) -> Result<()> {
        self.flush_cache().await?;
        self.writer.flush().await
    }

    async fn len(&self) -> Result<u64> {
        if !self.opened {
            if let Some(size) = self.total_size {
                return Ok(size);
            }
            return Ok(0);
        }
        self.writer.len().await
    }

    fn path(&self) -> &Path {
        &self.path
    }

    async fn close(&mut self) -> Result<()> {
        self.flush().await?;
        self.writer.close().await?;
        self.opened = false;
        Ok(())
    }
}

impl CachedDiskWriter {
    /// Flush all dirty cache entries to the underlying writer.
    ///
    /// Uses `CacheEntry::into_data()` to move the `Bytes` buffer out of each
    /// entry without copying - the bytes are passed directly to
    /// `write_bytes_at` which forwards to `pwrite` (zero-copy from cache to disk).
    async fn flush_cache(&mut self) -> Result<()> {
        if let Some(cache) = self.cache.clone() {
            cache.flush_to(self.writer.as_mut()).await?;
        }
        Ok(())
    }

    pub async fn read_all(&mut self) -> Result<Vec<u8>> {
        let len = self.len().await? as usize;
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; len];
        self.read_at(0, &mut buf).await?;
        Ok(buf)
    }
}
