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
pub(crate) fn advise_drop_cache(file: &impl std::os::fd::AsRawFd, offset: u64, length: u64) {
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
pub(crate) fn advise_drop_cache(_file: &impl std::os::fd::AsRawFd, _offset: u64, _length: u64) {}

pub struct DirectDiskAdaptor {
    file: Option<std::sync::Arc<std::fs::File>>,
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
        if let Some(file) = self.file.as_ref().cloned() {
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        advise_drop_cache(file.as_ref(), offset, data.len() as u64);
                        Ok(())
                    },
                    "drop disk cache hint",
                )
                .await?;
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
        let path = path.to_path_buf();
        let file = crate::filesystem::disk_io_pool::shared()
            .run(
                move || {
                    let mut open_opts = std::fs::OpenOptions::new();
                    open_opts.write(true).read(true);
                    if !path.exists() {
                        open_opts.create(true);
                    }
                    open_opts.open(&path).map_err(|e| {
                        crate::error::Aria2Error::FileOpen(format!("{}: {e}", path.display()))
                    })
                },
                "disk adaptor open",
            )
            .await?;
        self.file = Some(std::sync::Arc::new(file));

        Ok(())
    }

    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if let Some(file) = self.file.as_ref().cloned() {
            let data = data.to_vec();
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        crate::filesystem::positioned_disk_writer::platform_io::write_all_at(
                            file.as_ref(),
                            &data,
                            offset,
                        )
                    },
                    "disk adaptor write",
                )
                .await?;
        }
        Ok(())
    }

    async fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        if let Some(file) = self.file.as_ref().cloned() {
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        let mut buffer = vec![0u8; length as usize];
                        crate::filesystem::positioned_disk_writer::platform_io::read_exact_at(
                            file.as_ref(),
                            &mut buffer,
                            offset,
                        )?;
                        Ok(buffer)
                    },
                    "disk adaptor read",
                )
                .await
        } else {
            Err(crate::error::Aria2Error::DownloadFailed(
                "File not open".to_string(),
            ))
        }
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        drop(file);
                        Ok(())
                    },
                    "disk adaptor close",
                )
                .await?;
        }
        Ok(())
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        if let Some(file) = self.file.as_ref().cloned() {
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || file.set_len(length).map_err(crate::error::Aria2Error::from),
                    "disk adaptor truncate",
                )
                .await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    async fn size(&self) -> Result<u64> {
        if let Some(file) = self.file.as_ref().cloned() {
            crate::filesystem::disk_io_pool::shared()
                .run(
                    move || {
                        file.metadata()
                            .map(|metadata| metadata.len())
                            .map_err(crate::error::Aria2Error::from)
                    },
                    "disk adaptor metadata",
                )
                .await
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
