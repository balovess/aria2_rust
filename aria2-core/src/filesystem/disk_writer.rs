use async_trait::async_trait;
use std::path::Path;
use crate::error::Result;

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
