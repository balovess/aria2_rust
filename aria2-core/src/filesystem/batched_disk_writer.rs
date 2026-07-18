//! Batched disk writer: coalesces small writes into larger sequential flushes.
//!
//! This writer implements [`SeekableDiskWriter`] by buffering writes in a
//! `BTreeMap<u64, Vec<u8>>` and flushing them when a configurable threshold
//! (total buffered bytes or pending write count) is exceeded. This reduces the
//! number of `pwrite`/`seek_write` syscalls for workloads with many small,
//! non-contiguous writes.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tracing::debug;

use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_writer::SeekableDiskWriter;

pub struct BatchedDiskWriter {
    file: Option<tokio::fs::File>,
    path: PathBuf,
    buffer: BTreeMap<u64, Vec<u8>>,
    flush_threshold_bytes: usize,
    total_buffered: usize,
    max_pending_writes: usize,
    opened: bool,
}

impl BatchedDiskWriter {
    pub fn new(path: &Path) -> Self {
        Self {
            file: None,
            path: path.to_path_buf(),
            buffer: BTreeMap::new(),
            flush_threshold_bytes: 256 * 1024,
            total_buffered: 0,
            max_pending_writes: 16,
            opened: false,
        }
    }

    pub fn with_threshold(mut self, bytes: usize) -> Self {
        self.flush_threshold_bytes = bytes;
        self
    }

    pub fn with_max_pending(mut self, max: usize) -> Self {
        self.max_pending_writes = max;
        self
    }

    /// Open (or create) the file without truncating existing data.
    async fn ensure_open(&mut self) -> Result<()> {
        if !self.opened {
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .read(true)
                .open(&self.path)
                .await
                .map_err(|e| {
                    Aria2Error::Io(format!(
                        "Failed to open {}: {}",
                        self.path.display(),
                        e
                    ))
                })?;
            self.file = Some(f);
            self.opened = true;
        }
        Ok(())
    }

    fn should_flush(&self) -> bool {
        self.total_buffered >= self.flush_threshold_bytes
            || self.buffer.len() >= self.max_pending_writes
    }

    pub fn buffered_count(&self) -> usize {
        self.buffer.len()
    }

    pub fn buffered_bytes(&self) -> usize {
        self.total_buffered
    }
}

#[async_trait]
impl SeekableDiskWriter for BatchedDiskWriter {
    async fn open(&mut self) -> Result<()> {
        self.ensure_open().await
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.ensure_open().await?;

        if data.is_empty() {
            return Ok(());
        }

        self.buffer
            .entry(offset)
            .or_default()
            .extend_from_slice(data);
        self.total_buffered += data.len();

        if self.should_flush() {
            self.flush().await?;
        }

        Ok(())
    }

    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        self.write_at(offset, &data).await
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.ensure_open().await?;
        let file = self.file.as_mut().ok_or_else(|| {
            Aria2Error::Io("file not open after ensure_open — invariant violated".into())
        })?;
        use tokio::io::AsyncReadExt;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| Aria2Error::Io(format!("seek failed at offset {}: {}", offset, e)))?;
        let n = file
            .read(buf)
            .await
            .map_err(|e| Aria2Error::Io(format!("read failed at offset {}: {}", offset, e)))?;
        Ok(n)
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        self.ensure_open().await?;
        let file = self.file.as_mut().ok_or_else(|| {
            Aria2Error::Io("file not open after ensure_open — invariant violated".into())
        })?;
        file.set_len(length)
            .await
            .map_err(|e| Aria2Error::Io(format!("set_len({}) failed: {}", length, e)))
    }

    async fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        self.ensure_open().await?;
        let file = self.file.as_mut().ok_or_else(|| {
            Aria2Error::Io("file not open after ensure_open — invariant violated".into())
        })?;

        debug!(
            "[BatchedDiskWriter] Flushing {} writes ({} bytes)",
            self.buffer.len(),
            self.total_buffered
        );

        for (&offset, data) in self.buffer.iter() {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| Aria2Error::Io(format!("seek failed at offset {}: {}", offset, e)))?;
            file.write_all(data)
                .await
                .map_err(|e| Aria2Error::Io(format!("write failed at offset {}: {}", offset, e)))?;
        }

        file.flush()
            .await
            .map_err(|e| Aria2Error::Io(format!("flush failed: {}", e)))?;

        self.buffer.clear();
        self.total_buffered = 0;
        Ok(())
    }

    async fn len(&self) -> Result<u64> {
        match self.file.as_ref() {
            Some(f) => f
                .metadata()
                .await
                .map(|m| m.len())
                .map_err(|e| Aria2Error::Io(format!("metadata failed: {}", e))),
            None => Ok(0),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    async fn close(&mut self) -> Result<()> {
        self.flush().await?;
        if let Some(f) = self.file.take() {
            f.sync_all()
                .await
                .map_err(|e| Aria2Error::Io(format!("sync failed: {}", e)))?;
        }
        self.opened = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_new_writer() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let writer = BatchedDiskWriter::new(&path);

        assert!(!writer.opened);
        assert!(writer.file.is_none());
        assert_eq!(writer.buffered_count(), 0);
        assert_eq!(writer.buffered_bytes(), 0);
        assert_eq!(writer.path(), path.as_path());
    }

    #[tokio::test]
    async fn test_write_at_buffers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = BatchedDiskWriter::new(&path);

        writer.write_at(0, b"hello").await.unwrap();
        writer.write_at(100, b"world").await.unwrap();
        writer.write_at(200, b"!").await.unwrap();

        assert_eq!(writer.buffered_count(), 3);
        assert_eq!(writer.buffered_bytes(), 11);
        assert!(writer.opened);
    }

    #[tokio::test]
    async fn test_auto_flush_on_threshold() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = BatchedDiskWriter::new(&path).with_threshold(64);

        let large_data = vec![0xABu8; 128];
        writer.write_at(0, &large_data).await.unwrap();

        assert_eq!(writer.buffered_count(), 0);
        assert_eq!(writer.buffered_bytes(), 0);
    }

    #[tokio::test]
    async fn test_auto_flush_on_max_pending() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = BatchedDiskWriter::new(&path).with_max_pending(4);

        for i in 0..6u64 {
            writer.write_at(i * 1000, &[i as u8]).await.unwrap();
        }

        assert_eq!(writer.buffered_count(), 2);
    }

    #[tokio::test]
    async fn test_explicit_flush_writes_to_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = BatchedDiskWriter::new(&path).with_threshold(1024 * 1024);

        writer.write_at(0, b"hello ").await.unwrap();
        writer.write_at(6, b"world").await.unwrap();

        assert_eq!(writer.buffered_count(), 2);

        writer.flush().await.unwrap();
        assert_eq!(writer.buffered_count(), 0);

        let mut file = tokio::fs::File::open(&path).await.unwrap();
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello world");
    }

    #[tokio::test]
    async fn test_close_finalizes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = BatchedDiskWriter::new(&path);

        writer.write_at(0, b"data").await.unwrap();
        writer.close().await.unwrap();

        assert!(!writer.opened);
        assert!(writer.file.is_none());
        assert_eq!(writer.buffered_count(), 0);

        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.unwrap();
        assert_eq!(&buf, b"data");
    }

    #[tokio::test]
    async fn test_sequential_ordering() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = BatchedDiskWriter::new(&path).with_threshold(1024 * 1024);

        writer.write_at(100, b"B").await.unwrap();
        writer.write_at(50, b"A").await.unwrap();
        writer.write_at(200, b"C").await.unwrap();

        let offsets: Vec<u64> = writer.buffer.keys().copied().collect();
        assert_eq!(offsets, vec![50, 100, 200]);
    }

    #[tokio::test]
    async fn test_open_trait_method() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trait_open.bin");
        let mut writer = BatchedDiskWriter::new(&path);

        // Use the trait method
        SeekableDiskWriter::open(&mut writer).await.unwrap();
        assert!(writer.opened);
    }

    #[tokio::test]
    async fn test_truncate_seekable_trait() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("truncate_test.bin");
        let mut writer = BatchedDiskWriter::new(&path);

        writer.write_at(0, b"hello world").await.unwrap();
        writer.flush().await.unwrap();

        // Use the trait method
        SeekableDiskWriter::truncate(&mut writer, 5).await.unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 5);
    }

    #[tokio::test]
    async fn test_read_at_seekable_trait() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("read_test.bin");
        let mut writer = BatchedDiskWriter::new(&path);

        writer.write_at(0, b"hello world").await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = vec![0u8; 5];
        let n = SeekableDiskWriter::read_at(&mut writer, 6, &mut buf)
            .await
            .unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"world");
    }
}
