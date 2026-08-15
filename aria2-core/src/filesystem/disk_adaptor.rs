use crate::error::Result;
use async_trait::async_trait;
use std::any::Any;
use std::path::Path;

#[async_trait]
pub trait DiskAdaptor: Send + Sync {
    async fn open(&mut self, path: &Path) -> Result<()>;
    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    async fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>>;
    async fn close(&mut self) -> Result<()>;
    async fn truncate(&mut self, length: u64) -> Result<()>;
    async fn flush(&mut self) -> Result<()>;
    async fn size(&self) -> Result<u64>;
    fn as_any(&self) -> &dyn Any;

    #[cfg(unix)]
    fn unix_raw_fd(&self) -> Option<std::os::unix::io::RawFd>;

    /// Returns the raw OS file handle on Windows, or `None` if no file is open.
    /// The handle is borrowed (not owned); callers must not close it.
    #[cfg(windows)]
    fn windows_raw_handle(&self) -> Option<std::os::windows::io::RawHandle>;
}

/// Best-effort POSIX page-cache eviction for a file range.
///
/// This is an advisory hint: failure to evict pages must not change the bytes
/// returned to the caller. Non-POSIX callers simply omit the call.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
pub(crate) fn advise_drop_cache(file: &tokio::fs::File, offset: u64, length: u64) {
    use std::os::fd::AsRawFd;

    let Ok(offset) = libc::off_t::try_from(offset) else {
        return;
    };
    let Ok(length) = libc::off_t::try_from(length) else {
        return;
    };

    // SAFETY: `file` owns a live descriptor for this synchronous advisory
    // call. Both range values were checked to fit the platform's `off_t`.
    let _ =
        unsafe { libc::posix_fadvise(file.as_raw_fd(), offset, length, libc::POSIX_FADV_DONTNEED) };
}

// Apple libc does not expose the file-descriptor based `posix_fadvise` API.
// `posix_madvise` is not equivalent: it operates on a mapped memory range,
// not on a file descriptor. Cache eviction is only an advisory optimization,
// so preserve the read contract with a no-op on Apple Unix.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn advise_drop_cache(_file: &tokio::fs::File, _offset: u64, _length: u64) {}

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

    /// Read a range and ask the OS to drop the corresponding page-cache
    /// entries, matching aria2_original's `readDataDropCache` behavior.
    pub async fn read_data_drop_cache(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        let data = self.read(offset, length).await?;
        #[cfg(unix)]
        if let Some(file) = self.file.as_ref() {
            advise_drop_cache(file, offset, data.len() as u64);
        }
        Ok(data)
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

        self.file =
            Some(open_opts.open(path).await.map_err(|e| {
                crate::error::Aria2Error::FileOpen(format!("{}: {e}", path.display()))
            })?);

        Ok(())
    }

    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if let Some(ref mut file) = self.file {
            use tokio::io::{AsyncSeekExt, AsyncWriteExt};
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            file.write_all(data)
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        if let Some(ref mut file) = self.file {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            file.seek(std::io::SeekFrom::Start(offset))
                .await
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
                "File not open".to_string(),
            ))
        }
    }

    async fn close(&mut self) -> Result<()> {
        self.file = None;
        Ok(())
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        if let Some(ref mut file) = self.file {
            file.set_len(length)
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if let Some(ref mut file) = self.file {
            use tokio::io::AsyncWriteExt;
            file.flush()
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn size(&self) -> Result<u64> {
        if let Some(ref file) = self.file {
            let metadata = file
                .metadata()
                .await
                .map_err(|e| crate::error::Aria2Error::Io(e.to_string()))?;
            Ok(metadata.len())
        } else {
            Err(crate::error::Aria2Error::DownloadFailed(
                "File not open".to_string(),
            ))
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    #[cfg(unix)]
    fn unix_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        use std::os::fd::AsRawFd;
        self.file.as_ref().map(|f| f.as_raw_fd())
    }

    #[cfg(windows)]
    fn windows_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        use std::os::windows::io::AsRawHandle;
        self.file.as_ref().map(|f| f.as_raw_handle())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_data_drop_cache_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drop-cache.bin");
        tokio::fs::write(&path, b"drop cache data").await.unwrap();

        let mut adaptor = DirectDiskAdaptor::new();
        adaptor.open(&path).await.unwrap();
        let data = adaptor.read_data_drop_cache(5, 5).await.unwrap();
        assert_eq!(&data, b"cache");
        adaptor.close().await.unwrap();
    }
}
