use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::error::{Aria2Error, Result};

use super::disk_adaptor::{DiskAdaptor, DirectDiskAdaptor};
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
            self.file = Some(tokio::fs::File::create(&self.path).await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?);
        }
        
        if let Some(ref mut file) = self.file {
            use tokio::io::AsyncWriteExt;
            file.write_all(data).await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        if let Some(ref mut file) = self.file {
            use tokio::io::AsyncWriteExt;
            file.flush().await
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
        ByteArrayDiskWriter {
            buffer: Vec::new(),
        }
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
    fn default() -> Self { Self::new() }
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

    pub fn is_opened(&self) -> bool { self.opened }
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
                        tokio::fs::create_dir_all(parent).await
                            .map_err(|e: std::io::Error| Aria2Error::Io(e.to_string()))?;
                    }
                }
                adaptor.open(&self.path).await?;
                if let Some(size) = self.total_size {
                    if size > 0 {
                        adaptor.truncate(size).await?;
                    }
                }
            }
        }
        self.opened = true;
        Ok(())
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.open().await?;

        if data.len() >= self.direct_write_threshold {
            let mut adaptor = self.adaptor.lock().await;
            adaptor.write(offset, data).await?;
        } else if let Some(ref cache) = self.cache {
            cache.write(offset, data.to_vec()).await?;
        } else {
            let mut adaptor = self.adaptor.lock().await;
            adaptor.write(offset, data).await?;
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
        writer.write_at(0, b"hello world - this is longer than 20 bytes of data").await.unwrap();
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
}
