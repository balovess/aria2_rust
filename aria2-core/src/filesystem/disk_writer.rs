use crate::error::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

use super::disk_cache::WrDiskCache;
use super::mmap_disk_writer::MmapDiskWriter;
use super::positioned_disk_writer::PositionedDiskWriter;

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
            let f = tokio::fs::File::create(&self.path)
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            self.file = Some(f);
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
        if let Some(mut file) = self.file.take() {
            use tokio::io::AsyncWriteExt;
            file.flush()
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            // Close the file synchronously by converting to std::fs::File.
            // tokio::fs::File's Drop spawns a background close task, which on
            // Windows can leave the handle open briefly and cause "Access denied"
            // (os error 5) when the caller immediately reads the file.
            drop(file.into_std().await);
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

/// Fixed threshold: writes >= 1MB bypass the cache and go directly to disk.
const DIRECT_WRITE_THRESHOLD: usize = 1024 * 1024;

#[async_trait]
#[allow(clippy::len_without_is_empty)]
pub trait SeekableDiskWriter: Send + Sync {
    async fn open(&mut self) -> Result<()>;
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    /// Zero-copy write method accepting `bytes::Bytes` directly.
    /// This avoids the intermediate copy when the caller already has Bytes.
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        // Default implementation: delegate to write_at with slice
        self.write_at(offset, &data).await
    }
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize>;
    async fn truncate(&mut self, length: u64) -> Result<()>;
    async fn flush(&mut self) -> Result<()>;
    async fn len(&self) -> Result<u64>;
    fn path(&self) -> &Path;
    /// Close the writer and release underlying file resources.
    ///
    /// Default implementation is a no-op; implementors should override to
    /// truly release file handles and memory mappings. After `close`, the
    /// writer can be reopened with `open()`.
    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct CachedDiskWriter {
    /// The underlying positioned/mmap writer. Held as a trait object so the
    /// concrete strategy (PositionedDiskWriter vs MmapDiskWriter) can be
    /// selected at construction time. Unlike the legacy `Arc<Mutex<>>` design,
    /// there is NO internal async mutex — writes go directly to the writer,
    /// eliminating lock contention across `.await` points.
    writer: Box<dyn SeekableDiskWriter>,
    cache: Option<Arc<WrDiskCache>>,
    path: PathBuf,
    total_size: Option<u64>,
    opened: bool,
    // Rate limiter for write throttling
    rate_limiter: Option<Arc<crate::rate_limiter::RateLimiter>>,
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
        let writer: Box<dyn SeekableDiskWriter> = if use_mmap {
            Box::new(MmapDiskWriter::new(path, total_size))
        } else {
            Box::new(PositionedDiskWriter::new(path, total_size))
        };
        let cache = cache_size_mb.map(|mb| Arc::new(WrDiskCache::new(mb)));
        Self {
            writer,
            cache,
            path: path.to_path_buf(),
            total_size,
            opened: false,
            rate_limiter: None,
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
}

#[async_trait]
impl SeekableDiskWriter for CachedDiskWriter {
    async fn open(&mut self) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        // Delegate to the underlying writer (PositionedDiskWriter or
        // MmapDiskWriter). Both handle parent-dir creation, file creation,
        // and pre-allocation internally — no external logic needed here.
        self.writer.open().await?;
        self.opened = true;
        Ok(())
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.open().await?;

        // Rate limiting — non-blocking try_acquire
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
            // Large writes bypass the cache and go directly to the writer.
            self.writer.write_at(offset, data).await?;
        } else if let Some(ref cache) = self.cache {
            // Small writes go to the write-back cache.
            // copy_from_slice is unavoidable here: we only have a &[u8],
            // and the cache stores Bytes (Arc-backed).
            cache
                .write(offset, bytes::Bytes::copy_from_slice(data))
                .await?;
        } else {
            // No cache configured — write directly.
            self.writer.write_at(offset, data).await?;
        }

        Ok(())
    }

    /// Zero-copy write: accepts Bytes directly. When caching, the Bytes is
    /// moved into the cache (O(1) refcount bump). When direct-writing, the
    /// Bytes is passed by reference to pwrite (no copy).
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        self.open().await?;

        // Rate limiting — non-blocking try_acquire
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
            // Large writes bypass the cache — zero-copy to pwrite.
            self.writer.write_bytes_at(offset, data).await?;
        } else if let Some(ref cache) = self.cache {
            // Small writes go to the cache — zero-copy (move Bytes).
            cache.write(offset, data).await?;
        } else {
            // No cache configured — zero-copy to pwrite.
            self.writer.write_bytes_at(offset, data).await?;
        }

        Ok(())
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        // Flush any cached dirty entries before reading so the read sees
        // the most recent writes.
        self.flush_cache().await?;
        // The underlying writer reads directly into buf — no intermediate
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
    /// entry without copying — the bytes are passed directly to
    /// `write_bytes_at` which forwards to `pwrite` (zero-copy from cache to disk).
    async fn flush_cache(&mut self) -> Result<()> {
        if let Some(ref cache) = self.cache {
            let entries = cache.flush().await?;
            if !entries.is_empty() {
                for entry in entries {
                    let offset = entry.offset();
                    let data = entry.into_data();
                    if !data.is_empty() {
                        self.writer.write_bytes_at(offset, data).await?;
                    }
                }
                self.writer.flush().await?;
            }
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

        // Use smaller size to avoid disk space issues
        let large_data = vec![0xAB; 128 * 1024]; // 128KB instead of 256KB+

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

    // ── Rate limiter wiring tests ──────────────────────────

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

    // ── Concurrent write tests ─────────────────────

    #[tokio::test]
    async fn test_concurrent_writes_different_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_concurrent.bin");

        let mut writer = CachedDiskWriter::new(&path, Some(16 * 1024 * 1024), None);
        writer.open().await.unwrap();

        let mut handles = vec![];
        for i in 0..16 {
            let offset = (i as u64) * 1024 * 1024;
            let data = vec![i as u8; 4096];
            let path_clone = path.clone();

            handles.push(tokio::spawn(async move {
                let mut w = CachedDiskWriter::new(&path_clone, None, None);
                w.open().await.unwrap();
                w.write_at(offset, &data).await.unwrap();
                w.flush().await.unwrap();
                w.close().await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let content = tokio::fs::read(&path).await.unwrap();
        for i in 0..16 {
            let offset = (i as usize) * 1024 * 1024;
            let expected = vec![i as u8; 4096];
            assert_eq!(
                &content[offset..offset + 4096],
                &expected[..],
                "Data mismatch at offset {}",
                i
            );
        }
    }

    #[tokio::test]
    async fn test_concurrent_writes_serialized() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_same_offset.bin");

        let mut writer = CachedDiskWriter::new(&path, Some(1024 * 1024), None);
        writer.open().await.unwrap();
        writer.close().await.unwrap();

        let write_count = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        for i in 0..10 {
            let offset = (i as u64) * 1024;
            let data = vec![i as u8; 1024];
            let path_clone = path.clone();
            let counter = write_count.clone();

            handles.push(tokio::spawn(async move {
                let mut w = CachedDiskWriter::new(&path_clone, None, None);
                w.open().await.unwrap();
                w.write_at(offset, &data).await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                w.flush().await.unwrap();
                w.close().await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(write_count.load(Ordering::SeqCst), 10);

        let content = tokio::fs::read(&path).await.unwrap();
        for i in 0..10 {
            let offset = i * 1024;
            let expected = vec![i as u8; 1024];
            assert_eq!(
                &content[offset..offset + 1024],
                &expected[..],
                "Data mismatch at offset {}",
                offset
            );
        }
    }

    #[tokio::test]
    async fn test_high_concurrency_stress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_stress.bin");

        let mut writer = CachedDiskWriter::new(&path, Some(32 * 1024 * 1024), None);
        writer.open().await.unwrap();
        writer.close().await.unwrap();

        let num_threads = 32;
        let writes_per_thread = 100;
        let mut handles = vec![];

        for thread_id in 0..num_threads {
            let path_clone = path.clone();

            handles.push(tokio::spawn(async move {
                let mut w = CachedDiskWriter::new(&path_clone, None, None);
                w.open().await.unwrap();

                for write_id in 0..writes_per_thread {
                    let offset = ((thread_id * writes_per_thread + write_id) as u64) * 8192;
                    let data = vec![(thread_id + write_id) as u8; 8192];
                    w.write_at(offset, &data).await.unwrap();
                }

                w.flush().await.unwrap();
                w.close().await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let content = tokio::fs::read(&path).await.unwrap();
        for thread_id in 0..num_threads {
            for write_id in 0..writes_per_thread {
                let offset = ((thread_id * writes_per_thread + write_id) as usize) * 8192;
                let expected = vec![(thread_id + write_id) as u8; 8192];
                if offset + 8192 <= content.len() {
                    assert_eq!(
                        &content[offset..offset + 8192],
                        &expected[..],
                        "Data mismatch at thread {} write {}",
                        thread_id,
                        write_id
                    );
                }
            }
        }
    }

    /// Verify that 8 concurrent tasks writing 64 KiB chunks to non-overlapping
    /// offsets on a single `CachedDiskWriter` (wrapped in
    /// `Arc<tokio::sync::Mutex<>>`) complete without deadlock and with full
    /// data integrity.
    ///
    /// Since `write_at` takes `&mut self`, the external `tokio::sync::Mutex`
    /// serializes calls — but each call is now fast (no internal async mutex
    /// held across `.await` points), so 8 tasks should complete in roughly
    /// 1× single-write latency with no contention bottleneck.
    #[tokio::test]
    async fn test_concurrent_writes_no_mutex_contention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_no_contention.bin");

        let chunk_size: usize = 64 * 1024;
        let num_tasks: usize = 8;
        let total_size = (chunk_size * num_tasks) as u64;

        let mut writer = CachedDiskWriter::new(&path, Some(total_size), None);
        writer.open().await.unwrap();
        let writer = Arc::new(tokio::sync::Mutex::new(writer));

        let mut handles = Vec::with_capacity(num_tasks);
        for i in 0..num_tasks {
            let offset = (i as u64) * chunk_size as u64;
            let fill = (i as u8) + 1;
            let data = bytes::Bytes::from(vec![fill; chunk_size]);
            let w = writer.clone();
            handles.push(tokio::spawn(async move {
                let mut guard = w.lock().await;
                guard.write_bytes_at(offset, data).await.unwrap();
            }));
        }

        // If there were a deadlock, this join would hang forever.
        for handle in handles {
            handle.await.unwrap();
        }

        {
            let mut guard = writer.lock().await;
            guard.flush().await.unwrap();
        }

        // Verify data integrity: each chunk should contain its fill byte.
        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(content.len(), total_size as usize);
        for i in 0..num_tasks {
            let start = i * chunk_size;
            let expected = (i as u8) + 1;
            let chunk = &content[start..start + chunk_size];
            assert!(
                chunk.iter().all(|&b| b == expected),
                "data mismatch in task {} chunk",
                i
            );
        }
    }
}
