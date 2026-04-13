// aria2-core/src/engine/batched_disk_writer.rs

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tracing::debug;

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

    async fn ensure_open(&mut self) -> Result<(), String> {
        if !self.opened {
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .read(true)
                .open(&self.path)
                .await
                .map_err(|e| format!("Failed to open {}: {}", self.path.display(), e))?;
            self.file = Some(f);
            self.opened = true;
        }
        Ok(())
    }

    pub async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), String> {
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

    fn should_flush(&self) -> bool {
        self.total_buffered >= self.flush_threshold_bytes
            || self.buffer.len() >= self.max_pending_writes
    }

    pub async fn flush(&mut self) -> Result<(), String> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        self.ensure_open().await?;
        let file = self.file.as_mut().ok_or("File not open")?;

        debug!(
            "[BatchedDiskWriter] Flushing {} writes ({} bytes)",
            self.buffer.len(),
            self.total_buffered
        );

        for (&offset, data) in self.buffer.iter() {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| format!("seek failed at offset {}: {}", offset, e))?;
            file.write_all(data)
                .await
                .map_err(|e| format!("write failed at offset {}: {}", offset, e))?;
        }

        file.flush()
            .await
            .map_err(|e| format!("flush failed: {}", e))?;

        self.buffer.clear();
        self.total_buffered = 0;
        Ok(())
    }

    pub async fn close(&mut self) -> Result<(), String> {
        self.flush().await?;
        if let Some(f) = self.file.take() {
            f.sync_all()
                .await
                .map_err(|e| format!("sync failed: {}", e))?;
        }
        self.opened = false;
        Ok(())
    }

    pub fn buffered_count(&self) -> usize {
        self.buffer.len()
    }
    pub fn buffered_bytes(&self) -> usize {
        self.total_buffered
    }
    pub fn path(&self) -> &Path {
        &self.path
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
}
