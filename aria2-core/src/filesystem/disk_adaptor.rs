use crate::error::Result;
use async_trait::async_trait;
use std::any::Any;
use std::path::Path;

/// Logical file operations used by piece storage and multi-file downloads.
///
/// `MultiDiskAdaptor` implements global offsets that span torrent files. Single-file
/// I/O uses `PositionedDiskWriter` and the `SeekableDiskWriter` interface.
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
