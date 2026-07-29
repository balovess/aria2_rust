use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use tracing::debug;

use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_writer::SeekableDiskWriter;

use super::platform_io::{read_exact_at, write_all_at};

/// A disk writer that performs positioned (offset-based) I/O via OS-native
/// `pwrite`/`seek_write` syscalls.
///
/// The file descriptor is protected by a [`std::sync::Mutex`] held only for
/// the duration of each synchronous syscall — never across `.await` points.
/// This enables true concurrency for non-overlapping writes when multiple
/// writers reference the same file.
///
/// Uses [`std::fs::File`] (not `tokio::fs::File`) because `FileExt::write_at`
/// is a synchronous method available only on `std::fs::File`. Since `pwrite`
/// is a fast non-blocking syscall (it never waits on async I/O completion),
/// running it synchronously inside a tokio task is acceptable — it does not
/// stall the runtime for meaningful durations.
pub struct PositionedDiskWriter {
    /// `std::fs::File` wrapped in `Option` to support lazy open from `&self`.
    /// The `std::sync::Mutex` (NOT `tokio::sync::Mutex`) is intentional: the
    /// lock is held only for the synchronous syscall, never across await
    /// points, so it cannot deadlock the async runtime.
    file: Mutex<Option<std::fs::File>>,
    path: PathBuf,
    total_size: Option<u64>,
}

impl PositionedDiskWriter {
    /// Create a new `PositionedDiskWriter` for the given path.
    ///
    /// If `total_size` is provided and the file is newly created (size 0), the
    /// file is pre-allocated to `total_size` bytes on first open. This avoids
    /// fragmentation and enables concurrent writes to arbitrary offsets
    /// without per-write file extension.
    pub fn new(path: &Path, total_size: Option<u64>) -> Self {
        Self {
            file: Mutex::new(None),
            path: path.to_path_buf(),
            total_size,
        }
    }

    /// Returns the configured total size, if any.
    pub fn total_size(&self) -> Option<u64> {
        self.total_size
    }

    /// Lazily open the underlying file if not already open.
    ///
    /// This is synchronous because `pwrite`/`seek_write` are synchronous
    /// syscalls. The `std::sync::Mutex` is held only for the file open
    /// operation (microseconds), never across await points.
    ///
    /// Idempotent: if the file is already open, returns `Ok(())` immediately.
    fn ensure_open_sync(&self) -> Result<()> {
        let mut guard = self
            .file
            .lock()
            .map_err(|e| Aria2Error::Io(format!("file mutex poisoned: {e}")))?;
        if guard.is_some() {
            return Ok(());
        }

        // Create parent directories if needed (resume scenario may have missing dirs)
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
            debug!("Created parent directories for {:?}", self.path);
        }

        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).read(true);
        let file = opts.open(&self.path)?;
        debug!("Opened file for positioned I/O: {:?}", self.path);

        // Pre-allocate the file if a total size was specified and the file is
        // newly created (current size 0). For resume scenarios where the file
        // already has content, we do NOT truncate — preserving existing data.
        if let Some(size) = self.total_size {
            let current_size = file.metadata()?.len();
            if current_size == 0 && size > 0 {
                file.set_len(size)?;
                debug!("Pre-allocated file to {} bytes: {:?}", size, self.path);
            }
        }

        *guard = Some(file);
        Ok(())
    }

    /// Acquire the file mutex guard, returning a descriptive error if poisoned.
    fn lock_file(&self) -> Result<std::sync::MutexGuard<'_, Option<std::fs::File>>> {
        self.file
            .lock()
            .map_err(|e| Aria2Error::Io(format!("file mutex poisoned: {e}")))
    }

    /// Returns the raw file descriptor of the underlying file, if open.
    ///
    /// This is used by the Linux splice download path to obtain the file fd
    /// for `splice(2)` zero-copy transfer. Returns `None` if the file has not
    /// been opened yet (caller should call `open()` first).
    ///
    /// The caller must ensure the writer is not dropped or closed while the
    /// returned fd is in use. The fd is valid only while the writer holds the
    /// file open.
    #[cfg(unix)]
    pub fn raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        let guard = self.file.lock().ok()?;
        guard.as_ref().map(std::os::unix::io::AsRawFd::as_raw_fd)
    }
}

#[async_trait]
impl SeekableDiskWriter for PositionedDiskWriter {
    async fn open(&mut self) -> Result<()> {
        self.ensure_open_sync()
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.ensure_open_sync()?;
        let guard = self.lock_file()?;
        let file = guard.as_ref().ok_or_else(|| {
            Aria2Error::Io("file not open after ensure_open_sync — invariant violated".into())
        })?;
        write_all_at(file, data, offset)
    }

    /// Zero-copy write: accepts `Bytes` directly. Since `pwrite` takes `&[u8]`,
    /// we simply dereference the `Bytes` (no copy — `Bytes` derefs to `[u8]`).
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        self.write_at(offset, &data).await
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.ensure_open_sync()?;
        let guard = self.lock_file()?;
        let file = guard.as_ref().ok_or_else(|| {
            Aria2Error::Io("file not open after ensure_open_sync — invariant violated".into())
        })?;
        read_exact_at(file, buf, offset)
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        self.ensure_open_sync()?;
        let guard = self.lock_file()?;
        if let Some(ref file) = *guard {
            file.set_len(length)?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        let guard = self.lock_file()?;
        if let Some(ref _file) = *guard {
            // Do NOT call sync_all (fsync) here — that is only needed on
            // close/finalize to guarantee durability. The download hot path
            // calls flush() to push data from the write-back cache to the
            // kernel page cache (via pwrite), and sync_all would force a
            // disk barrier on every cache flush, drastically reducing
            // throughput on SSDs and especially HDDs.
            //
            // pwrite already places data in the kernel page cache where it
            // is visible to other readers and safe from process crashes.
            // sync_all is deferred to `close()`.
            //
            // If callers need explicit durability (e.g., session save),
            // they should call `close()` instead of `flush()`.
        }
        Ok(())
    }

    async fn len(&self) -> Result<u64> {
        let guard = self.lock_file()?;
        if let Some(ref file) = *guard {
            Ok(file.metadata()?.len())
        } else if let Some(size) = self.total_size {
            Ok(size)
        } else {
            Ok(0)
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    async fn close(&mut self) -> Result<()> {
        let guard = self.lock_file()?;
        if let Some(ref file) = *guard {
            // Ensure all buffered data reaches stable storage before closing.
            // This is the ONLY place sync_all (fsync) is called — the hot-path
            // flush() intentionally skips it for throughput.
            file.sync_all()?;
        }
        // Drop the file handle by taking it out of the Option
        drop(guard);
        let mut guard = self.lock_file()?;
        *guard = None;
        Ok(())
    }
}
