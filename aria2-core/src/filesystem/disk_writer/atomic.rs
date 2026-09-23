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
    file: Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>,
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
        if self.file.is_none() {
            let path = self.path.clone();
            let preserve_existing = self.write_offset.is_some();
            let file = crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        let file = if preserve_existing {
                            std::fs::OpenOptions::new()
                                .write(true)
                                .create(true)
                                .truncate(false)
                                .open(path)
                        } else {
                            std::fs::File::create(path)
                        }
                        .map_err(crate::error::Aria2Error::from)?;
                        Ok(file)
                    },
                    "sequential writer open",
                )
                .await?;
            self.file = Some(std::sync::Arc::new(std::sync::Mutex::new(file)));
        }
        if let Some(file) = self.file.as_ref().cloned() {
            let data = data.to_vec();
            let bytes_written = data.len() as u64;
            let offset = self.write_offset;
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        use std::io::{Seek, Write};
                        let mut file = file
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(offset) = offset {
                            file.seek(std::io::SeekFrom::Start(offset))
                                .map_err(crate::error::Aria2Error::from)?;
                        }
                        file.write_all(&data)
                            .map_err(crate::error::Aria2Error::from)
                    },
                    "sequential write",
                )
                .await?;
            if let Some(offset) = &mut self.write_offset {
                *offset = offset.saturating_add(bytes_written);
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if let Some(file) = self.file.as_ref().cloned() {
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        use std::io::Write;
                        let mut file = file
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        file.flush().map_err(crate::error::Aria2Error::from)?;
                        file.sync_data().map_err(crate::error::Aria2Error::from)
                    },
                    "sequential writer flush",
                )
                .await?;
        }
        Ok(())
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        if let Some(file) = self.file.take() {
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        use std::io::Write;
                        let mut file = file
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        file.flush().map_err(crate::error::Aria2Error::from)?;
                        file.sync_all().map_err(crate::error::Aria2Error::from)
                    },
                    "sequential writer finalize",
                )
                .await?;
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

    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.buffer))
    }
}
