//! Atomic (non-buffered) disk writers.
//!
//! - [`DefaultDiskWriter`] - direct file writer (sequential writes to a file on disk)
//! - [`ByteArrayDiskWriter`] - in-memory byte buffer writer (no I/O)

use super::DiskWriter;
use crate::error::Result;
use async_trait::async_trait;
use std::path::Path;

// -- DefaultDiskWriter -------------------------------------------------------

pub struct DefaultDiskWriter {
    path: std::path::PathBuf,
    file: Option<tokio::fs::File>,
    write_offset: Option<u64>,
}

impl DefaultDiskWriter {
    pub fn new(path: &Path) -> Self {
        DefaultDiskWriter {
            path: path.to_path_buf(),
            file: None,
            write_offset: None,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn new_with_offset(path: &Path, offset: u64) -> Self {
        Self {
            path: path.to_path_buf(),
            file: None,
            write_offset: Some(offset),
        }
    }
}

#[async_trait]
impl DiskWriter for DefaultDiskWriter {
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

        if self.file.is_none() {
            let file = if self.write_offset.is_some() {
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .open(&self.path)
                    .await
            } else {
                tokio::fs::File::create(&self.path).await
            };
            self.file = Some(file.map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?);
        }
        if let Some(ref mut file) = self.file {
            if let Some(offset) = self.write_offset {
                file.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            }
            file.write_all(data)
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            if let Some(offset) = &mut self.write_offset {
                *offset = offset.saturating_add(data.len() as u64);
            }
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

// -- ByteArrayDiskWriter -----------------------------------------------------

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
