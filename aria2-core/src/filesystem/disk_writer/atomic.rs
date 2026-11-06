//! Atomic (non-buffered) disk writers.
//!
//! - [`DefaultDiskWriter`] - direct file writer (sequential writes to a file on disk)
//! - [`ByteArrayDiskWriter`] - in-memory byte buffer writer (no I/O)

use super::DiskWriter;
use crate::error::{Aria2Error, Result};
use async_trait::async_trait;
use std::path::Path;

// -- DefaultDiskWriter -------------------------------------------------------

pub struct DefaultDiskWriter {
    path: std::path::PathBuf,
    file: Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>,
    write_offset: Option<u64>,
    namespace_sync: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl DefaultDiskWriter {
    pub fn new(path: &Path) -> Self {
        DefaultDiskWriter {
            path: path.to_path_buf(),
            file: None,
            write_offset: None,
            namespace_sync: None,
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
            namespace_sync: None,
        }
    }

    fn start_namespace_sync(&mut self) {
        if self.namespace_sync.is_none() {
            let path = self.path.clone();
            self.namespace_sync = Some(tokio::spawn(async move {
                crate::filesystem::durability::sync_parent_directories(&path).await
            }));
        }
    }

    async fn finish_namespace_sync(&mut self) -> Result<()> {
        if let Some(sync) = self.namespace_sync.take() {
            sync.await.map_err(|error| {
                Aria2Error::Io(format!("directory sync task failed: {error}"))
            })??;
        }
        Ok(())
    }

    async fn sync_file(&self, include_metadata: bool) -> Result<()> {
        if let Some(file) = self.file.as_ref().cloned() {
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        use std::io::Write;
                        let mut file = file
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        file.flush().map_err(crate::error::Aria2Error::from)?;
                        if include_metadata {
                            file.sync_all().map_err(crate::error::Aria2Error::from)
                        } else {
                            file.sync_data().map_err(crate::error::Aria2Error::from)
                        }
                    },
                    "sequential writer sync",
                )
                .await?;
        }
        Ok(())
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
            self.start_namespace_sync();
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
        self.sync_file(true).await?;
        self.finish_namespace_sync().await
    }

    async fn sync_data(&mut self) -> Result<()> {
        self.sync_file(false).await?;
        self.finish_namespace_sync().await
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        if self.file.is_some() {
            self.sync_file(true).await?;
        }
        self.file.take();
        self.finish_namespace_sync().await?;
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

    async fn sync_data(&mut self) -> Result<()> {
        Ok(())
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.buffer))
    }
}
