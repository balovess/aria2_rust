use async_trait::async_trait;
use std::path::Path;
use crate::error::{Aria2Error, Result};

#[async_trait]
pub trait DiskAdaptor: Send + Sync {
    async fn open(&mut self, path: &Path) -> Result<()>;
    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    async fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>>;
    async fn close(&mut self) -> Result<()>;
    async fn truncate(&mut self, length: u64) -> Result<()>;
    async fn flush(&mut self) -> Result<()>;
    async fn size(&self) -> Result<u64>;
}

pub struct DirectDiskAdaptor {
    file: Option<tokio::fs::File>,
    path: std::path::PathBuf,
}

impl DirectDiskAdaptor {
    pub fn new() -> Self {
        DirectDiskAdaptor {
            file: None,
            path: std::path::PathBuf::new(),
        }
    }
}

impl Default for DirectDiskAdaptor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DiskAdaptor for DirectDiskAdaptor {
    async fn open(&mut self, path: &Path) -> Result<()> {
        self.path = path.to_path_buf();
        let mut open_opts = tokio::fs::OpenOptions::new();
        
        if path.exists() {
            open_opts.write(true).read(true);
        } else {
            open_opts.write(true).create(true).read(true);
        }
        
        self.file = Some(open_opts.open(path).await
            .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?);
        
        Ok(())
    }

    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if let Some(ref mut file) = self.file {
            use tokio::io::{AsyncSeekExt, AsyncWriteExt};
            file.seek(std::io::SeekFrom::Start(offset)).await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            file.write_all(data).await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        if let Some(ref mut file) = self.file {
            use tokio::io::{AsyncSeekExt, AsyncReadExt};
            file.seek(std::io::SeekFrom::Start(offset)).await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            
            let mut buffer = vec![0u8; length as usize];
            let bytes_read = file.read_exact(&mut buffer).await;
            
            match bytes_read {
                Ok(_) => Ok(buffer),
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        Ok(buffer)
                    } else {
                        Err(crate::error::Aria2Error::Io(e.to_string()))
                    }
                }
            }
        } else {
            Err(crate::error::Aria2Error::DownloadFailed(
                "文件未打开".to_string()
            ))
        }
    }

    async fn close(&mut self) -> Result<()> {
        self.file = None;
        Ok(())
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        if let Some(ref mut file) = self.file {
            use tokio::io::AsyncSeekExt;
            file.set_len(length).await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if let Some(ref mut file) = self.file {
            use tokio::io::AsyncWriteExt;
            file.flush().await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn size(&self) -> Result<u64> {
        if let Some(ref file) = self.file {
            let metadata = file.metadata().await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            Ok(metadata.len())
        } else {
            Err(crate::error::Aria2Error::DownloadFailed(
                "文件未打开".to_string()
            ))
        }
    }
}
