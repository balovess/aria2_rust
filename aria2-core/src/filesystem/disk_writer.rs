use crate::error::{Aria2Error, Result};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::debug;

use super::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use super::disk_cache::WrDiskCache;

#[async_trait]
pub trait DiskWriter: Send + Sync {
    async fn write(&mut self, data: &[u8]) -> Result<()>;
    async fn finalize(&mut self) -> Result<Vec<u8>>;
}

pub struct DefaultDiskWriter {
    path: std::path::PathBuf,
    file: Option<tokio::fs::File>,
}

impl DefaultDiskWriter {
    pub fn new(path: &Path) -> Self {
        DefaultDiskWriter {
            path: path.to_path_buf(),
            file: None,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[async_trait]
impl DiskWriter for DefaultDiskWriter {
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        if self.file.is_none() {
            self.file = Some(
                tokio::fs::File::create(&self.path)
                    .await
                    .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?,
            );
        }

        if let Some(ref mut file) = self.file {
            use tokio::io::AsyncWriteExt;
            file.write_all(data)
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        if let Some(ref mut file) = self.file {
            use tokio::io::AsyncWriteExt;
            file.flush()
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(vec![])
    }
}

pub struct ByteArrayDiskWriter {
    buffer: Vec<u8>,
}

impl ByteArrayDiskWriter {
    pub fn new() -> Self {
        ByteArrayDiskWriter { buffer: Vec::new() }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        ByteArrayDiskWriter {
            buffer: Vec::with_capacity(capacity),
        }
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

impl Default for ByteArrayDiskWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DiskWriter for ByteArrayDiskWriter {
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        let buffer = self.buffer.clone();
        Ok(buffer)
    }
}

const DEFAULT_DIRECT_WRITE_THRESHOLD: usize = 256 * 1024;

#[async_trait]
#[allow(clippy::len_without_is_empty)]
pub trait SeekableDiskWriter: Send + Sync {
    async fn open(&mut self) -> Result<()>;
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize>;
    async fn truncate(&mut self, length: u64) -> Result<()>;
    async fn flush(&mut self) -> Result<()>;
    async fn len(&self) -> Result<u64>;
    fn path(&self) -> &Path;
}

pub struct CachedDiskWriter {
    adaptor: Arc<Mutex<DirectDiskAdaptor>>,
    cache: Option<Arc<WrDiskCache>>,
    path: PathBuf,
    total_size: Option<u64>,
    direct_write_threshold: usize,
    opened: bool,
    // Task I2: Rate limiter for write throttling
    rate_limiter: Option<Arc<crate::rate_limiter::RateLimiter>>,
    // Task I3: Adaptive threshold fields
    write_history: VecDeque<usize>,
    write_count: usize,
}

impl CachedDiskWriter {
    pub fn new(path: &Path, total_size: Option<u64>, cache_size_mb: Option<usize>) -> Self {
        let cache = cache_size_mb.map(|mb| Arc::new(WrDiskCache::new(mb)));
        Self {
            adaptor: Arc::new(Mutex::new(DirectDiskAdaptor::new())),
            cache,
            path: path.to_path_buf(),
            total_size,
            direct_write_threshold: DEFAULT_DIRECT_WRITE_THRESHOLD,
            opened: false,
            rate_limiter: None,
            write_history: VecDeque::with_capacity(100),
            write_count: 0,
        }
    }

    pub fn open_existing(path: &Path) -> Result<Self> {
        let mut writer = Self::new(path, None, None);
        writer.opened = true;
        Ok(writer)
    }

    pub fn with_threshold(mut self, threshold: usize) -> Self {
        self.direct_write_threshold = threshold;
        self
    }

    /// Attach a rate limiter for write throttling (Task I2).
    /// Uses non-blocking try_acquire: if tokens unavailable, writes proceed without blocking.
    pub fn with_rate_limiter(mut self, limiter: Arc<crate::rate_limiter::RateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    pub fn is_opened(&self) -> bool {
        self.opened
    }

    /// Returns the current direct_write_threshold value.
    /// Useful for observing adaptive threshold adjustments (Task I3).
    pub fn direct_write_threshold(&self) -> usize {
        self.direct_write_threshold
    }

    /// Returns total number of writes recorded since creation or last reset.
    pub fn write_count(&self) -> usize {
        self.write_count
    }
}

#[async_trait]
impl SeekableDiskWriter for CachedDiskWriter {
    async fn open(&mut self) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        {
            let mut adaptor = self.adaptor.lock().await;
            if self.path.exists() {
                adaptor.open(&self.path).await?;
            } else {
                if let Some(parent) = self.path.parent() {
                    let parent: &Path = parent;
                    if !parent.exists() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .map_err(|e: std::io::Error| Aria2Error::Io(e.to_string()))?;
                    }
                }
                adaptor.open(&self.path).await?;
                if let Some(size) = self.total_size
                    && size > 0
                {
                    adaptor.truncate(size).await?;
                }
            }
        }
        self.opened = true;
        Ok(())
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.open().await?;

        // Task I2: Rate limiting — non-blocking try_acquire
        if let Some(ref limiter) = self.rate_limiter
            && !limiter.try_acquire_download(data.len() as u64).await
        {
            debug!(
                "Rate limit exceeded for {} bytes at offset {}, writing without throttling",
                data.len(),
                offset
            );
        }

        let write_len = data.len();

        if data.len() >= self.direct_write_threshold {
            let mut adaptor = self.adaptor.lock().await;
            adaptor.write(offset, data).await?;
        } else if let Some(ref cache) = self.cache {
            cache.write(offset, data.to_vec()).await?;
        } else {
            let mut adaptor = self.adaptor.lock().await;
            adaptor.write(offset, data).await?;
        }

        // Task I3: Track write size for adaptive threshold
        self.write_history.push_back(write_len);
        self.write_count += 1;

        // Adapt threshold every 100 writes
        if self.write_count.is_multiple_of(100) && self.write_history.len() >= 10 {
            let mut sorted: Vec<usize> = self.write_history.iter().copied().collect();
            sorted.sort_unstable();
            let p90_idx = (sorted.len() as f64 * 0.9) as usize;
            let p90 = sorted
                .get(p90_idx.min(sorted.len() - 1))
                .copied()
                .unwrap_or(DEFAULT_DIRECT_WRITE_THRESHOLD);
            // Clamp between 64KB and 4MB
            self.direct_write_threshold = p90.clamp(64 * 1024, 4 * 1024 * 1024);
            debug!(
                "Adaptive direct_write_threshold adjusted to {} bytes (p90={})",
                self.direct_write_threshold, p90
            );
        }

        Ok(())
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.flush_cache().await?;

        let mut adaptor = self.adaptor.lock().await;
        let data: Vec<u8> = adaptor.read(offset, buf.len() as u64).await?;
        let copy_len = data.len().min(buf.len());
        buf[..copy_len].copy_from_slice(&data[..copy_len]);
        Ok(copy_len)
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        self.flush_cache().await?;

        let mut adaptor = self.adaptor.lock().await;
        adaptor.truncate(length).await
    }

    async fn flush(&mut self) -> Result<()> {
        self.flush_cache().await?;

        let mut adaptor = self.adaptor.lock().await;
        adaptor.flush().await
    }

    async fn len(&self) -> Result<u64> {
        if !self.opened {
            if let Some(size) = self.total_size {
                return Ok(size);
            }
            return Ok(0);
        }

        let adaptor = self.adaptor.lock().await;
        adaptor.size().await
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl CachedDiskWriter {
    async fn flush_cache(&self) -> Result<()> {
        if let Some(ref cache) = self.cache {
            let entries = cache.flush().await?;
            if !entries.is_empty() {
                let mut adaptor = self.adaptor.lock().await;
                for entry in &entries {
                    if entry.is_dirty() || !entry.data().is_empty() {
                        adaptor.write(entry.offset(), entry.data()).await?;
                    }
                }
                adaptor.flush().await?;
            }
        }
        Ok(())
    }

    pub async fn close(&mut self) -> Result<()> {
        self.flush().await?;
        let mut adaptor = self.adaptor.lock().await;
        adaptor.close().await?;
        self.opened = false;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_default_disk_writer_write_and_finalize() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_default.bin");

        let mut writer = DefaultDiskWriter::new(&path);
        writer.write(b"hello").await.unwrap();
        writer.write(b" world").await.unwrap();
        writer.finalize().await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_byte_array_disk_writer() {
        let mut writer = ByteArrayDiskWriter::with_capacity(10);
        writer.write(b"abc").await.unwrap();
        writer.write(b"def").await.unwrap();
        let result = writer.finalize().await.unwrap();
        assert_eq!(result, b"abcdef");
        assert_eq!(writer.len(), 6);
    }

    #[tokio::test]
    async fn test_seekable_writer_basic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_seekable.bin");

        let mut writer = CachedDiskWriter::new(&path, Some(1024), None);
        writer.open().await.unwrap();
        assert!(writer.is_opened());

        writer.write_at(0, b"hello").await.unwrap();
        writer.write_at(5, b" world").await.unwrap();
        writer.flush().await.unwrap();

        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(&content[..11], b"hello world");
    }

    #[tokio::test]
    async fn test_seekable_writer_random_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_random.bin");

        let mut writer = CachedDiskWriter::new(&path, None, None);
        writer.open().await.unwrap();

        writer.write_at(200, b"SEG2").await.unwrap();
        writer.write_at(0, b"SEG0").await.unwrap();
        writer.write_at(100, b"SEG1").await.unwrap();
        writer.flush().await.unwrap();

        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(content.len(), 204);
        assert_eq!(&content[0..4], b"SEG0");
        assert_eq!(&content[100..104], b"SEG1");
        assert_eq!(&content[200..204], b"SEG2");
    }

    #[tokio::test]
    async fn test_seekable_writer_read_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_read.bin");

        let mut writer = CachedDiskWriter::new(&path, Some(100), None);
        writer.open().await.unwrap();
        writer.write_at(50, b"offset-50-data").await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 14];
        let n = writer.read_at(50, &mut buf).await.unwrap();
        assert_eq!(n, 14);
        assert_eq!(&buf, b"offset-50-data");
    }

    #[tokio::test]
    async fn test_cached_writer_with_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_cached.bin");

        let mut writer = CachedDiskWriter::new(&path, Some(4096), Some(1));
        writer.open().await.unwrap();

        for i in 0..100 {
            let data = vec![i as u8; 64];
            writer.write_at((i * 64) as u64, &data).await.unwrap();
        }

        writer.flush().await.unwrap();

        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(content.len(), 6400);

        for i in 0..100 {
            let start = i * 64;
            assert_eq!(content[start], i as u8, "mismatch at byte {}", start);
        }
    }

    #[tokio::test]
    async fn test_cached_writer_large_write_bypasses_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_large.bin");

        let large_data = vec![0xAB; DEFAULT_DIRECT_WRITE_THRESHOLD + 1];

        let mut writer = CachedDiskWriter::new(&path, None, Some(1));
        writer.open().await.unwrap();
        writer.write_at(0, &large_data).await.unwrap();
        writer.flush().await.unwrap();

        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(content.len(), large_data.len());
        assert!(content.iter().all(|&b| b == 0xAB));
    }

    #[tokio::test]
    async fn test_seekable_writer_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_trunc.bin");

        let mut writer = CachedDiskWriter::new(&path, Some(1000), None);
        writer.open().await.unwrap();
        writer
            .write_at(0, b"hello world - this is longer than 20 bytes of data")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        writer.truncate(20).await.unwrap();
        writer.flush().await.unwrap();

        let len = writer.len().await.unwrap();
        assert!(len <= 21);

        let content = tokio::fs::read(&path).await.unwrap();
        assert!(content.len() <= 21);
        assert_eq!(&content[..4], b"hell");
    }

    #[tokio::test]
    async fn test_seekable_writer_len_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_len.bin");

        let writer = CachedDiskWriter::new(&path, Some(9999), None);
        let len = writer.len().await.unwrap();
        assert_eq!(len, 9999);
    }

    #[tokio::test]
    async fn test_close_reopens_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_close.bin");

        let mut writer = CachedDiskWriter::new(&path, None, None);
        writer.open().await.unwrap();
        writer.write_at(0, b"before close").await.unwrap();
        writer.close().await.unwrap();
        assert!(!writer.is_opened());

        writer.open().await.unwrap();
        writer.write_at(12, b" after reopen").await.unwrap();
        writer.close().await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "before close after reopen");
    }

    // ── Task I2: Rate limiter wiring tests ──────────────────────────

    #[tokio::test]
    async fn test_cached_writer_with_rate_limiter() {
        use crate::rate_limiter::{RateLimiter, RateLimiterConfig};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_ratelimited.bin");

        // Create a very restrictive limiter (10 bytes/sec, tiny burst)
        let cfg = RateLimiterConfig::new(Some(10), None).with_burst(Some(20), None);
        let rl = Arc::new(RateLimiter::new(&cfg));

        let mut writer =
            CachedDiskWriter::new(&path, Some(4096), None).with_rate_limiter(rl.clone());
        writer.open().await.unwrap();

        // Write data — should succeed (try_acquire may fail but we still write)
        let data = vec![0x42u8; 512];
        writer.write_at(0, &data).await.unwrap();
        writer.flush().await.unwrap();

        let content = tokio::fs::read(&path).await.unwrap();
        assert!(content.len() >= 512, "file should be at least 512 bytes");
        assert_eq!(&content[..512], &vec![0x42u8; 512][..]);
        assert!(content.iter().take(512).all(|&b| b == 0x42));
    }

    #[tokio::test]
    async fn test_cached_writer_without_rate_limiter_no_effect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_nolimiter.bin");

        // No rate limiter attached — default behaviour
        let mut writer = CachedDiskWriter::new(&path, Some(1024), None);
        writer.open().await.unwrap();
        writer.write_at(0, b"no limiter").await.unwrap();
        writer.flush().await.unwrap();

        let content = tokio::fs::read(&path).await.unwrap();
        assert!(
            content.starts_with(b"no limiter"),
            "should contain written data"
        );
    }

    // ── Task I3: Adaptive threshold tests ─────────────────────────

    #[tokio::test]
    async fn test_adaptive_threshold_increases_with_small_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_adapt_up.bin");

        // Start with low threshold so small writes go to cache
        let mut writer =
            CachedDiskWriter::new(&path, Some(1024 * 1024), None).with_threshold(64 * 1024); // 64KB initial
        writer.open().await.unwrap();

        assert_eq!(writer.direct_write_threshold(), 64 * 1024);

        // Do 100+ small writes of 1KB each
        for i in 0..110 {
            let data = vec![i as u8; 1024];
            writer.write_at((i * 1024) as u64, &data).await.unwrap();
        }

        // After 100 writes, threshold should have adapted upward
        // p90 of [1024, 1024, ...] is 1024, clamped to min 64KB → stays at 64KB
        // But the mechanism itself should have fired (write_count >= 100)
        assert_eq!(writer.write_count(), 110);
        // Threshold should be clamped between 64KB and 4MB
        let thresh = writer.direct_write_threshold();
        assert!(
            thresh >= 64 * 1024,
            "threshold {} should be >= 64KB",
            thresh
        );
        assert!(
            thresh <= 4 * 1024 * 1024,
            "threshold {} should be <= 4MB",
            thresh
        );
    }

    #[tokio::test]
    async fn test_adaptive_threshold_decreases_with_large_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_adapt_down.bin");

        // Start with a high threshold
        let mut writer = CachedDiskWriter::new(&path, Some(10 * 1024 * 1024), None)
            .with_threshold(4 * 1024 * 1024);
        writer.open().await.unwrap();

        assert_eq!(writer.direct_write_threshold(), 4 * 1024 * 1024);

        // Do 100+ writes of varying sizes including some small ones
        // Mix of sizes: mostly small (1KB-8KB) with occasional larger ones
        for i in 0..110 {
            let size = if i % 10 == 0 { 64 * 1024 } else { 2 * 1024 }; // 90% are 2KB
            let data = vec![i as u8; size];
            writer
                .write_at((i * size as usize) as u64, &data)
                .await
                .unwrap();
        }

        assert_eq!(writer.write_count(), 110);

        // p90 should be on the lower end since most writes are small (2KB)
        // Clamped minimum is 64KB
        let thresh = writer.direct_write_threshold();
        assert!(
            thresh >= 64 * 1024,
            "adaptive threshold {} should be >= 64KB floor",
            thresh
        );
        assert!(
            thresh <= 4 * 1024 * 1024,
            "adaptive threshold {} should be <= 4MB ceiling",
            thresh
        );
    }

    #[tokio::test]
    async fn test_adaptive_threshold_clamping_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_clamp.bin");

        let mut writer = CachedDiskWriter::new(&path, Some(1024), None);
        writer.open().await.unwrap();

        // Write 100 entries of exactly 128 bytes each
        for i in 0..100 {
            let data = vec![i as u8; 128];
            writer.write_at((i * 128) as u64, &data).await.unwrap();
        }

        // p90 of all-128-byte writes is 128, but threshold must be >= 64KB
        let thresh = writer.direct_write_threshold();
        assert_eq!(
            thresh,
            64 * 1024,
            "should clamp to 64KB floor when p90 is tiny"
        );
    }
}
