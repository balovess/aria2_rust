use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tracing::debug;

use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_writer::SeekableDiskWriter;

use super::platform_io::{read_exact_at, write_all_at};

/// A disk writer that performs positioned (offset-based) I/O via OS-native
/// `pwrite`/`seek_write` syscalls.
///
/// The file descriptor is shared through an [`Arc`], and blocking filesystem
/// calls are dispatched to Tokio's blocking pool. Positioned reads and writes
/// do not mutate the file cursor, so they do not need a per-write mutex.
///
/// Uses [`std::fs::File`] (not `tokio::fs::File`) because `FileExt::write_at`
/// is a synchronous method available only on `std::fs::File`. The syscall is
/// nevertheless allowed to wait on filesystem pressure, so it must not run on
/// a Tokio worker thread.
pub struct PositionedDiskWriter {
    /// `std::fs::File` is wrapped in `Option` to support lazy opening.
    file: Option<Arc<std::fs::File>>,
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
            file: None,
            path: path.to_path_buf(),
            total_size,
        }
    }

    /// Returns the configured total size, if any.
    pub fn total_size(&self) -> Option<u64> {
        self.total_size
    }

    /// Lazily open the underlying file on Tokio's blocking pool.
    async fn ensure_open(&mut self) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }

        let path = self.path.clone();
        let total_size = self.total_size;
        let file = tokio::task::spawn_blocking(move || open_file(&path, total_size))
            .await
            .map_err(|e| Aria2Error::Io(format!("positioned writer open task failed: {e}")))??;
        self.file = Some(Arc::new(file));
        Ok(())
    }

    fn file_handle(&self) -> Result<Arc<std::fs::File>> {
        self.file.as_ref().cloned().ok_or_else(|| {
            Aria2Error::Io("file not open after ensure_open - invariant violated".into())
        })
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
        self.file
            .as_ref()
            .map(std::os::unix::io::AsRawFd::as_raw_fd)
    }
}

fn open_file(path: &Path, total_size: Option<u64>) -> Result<std::fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| Aria2Error::DirCreate(format!("{}: {e}", parent.display())))?;
        debug!("Created parent directories for {:?}", path);
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
        .map_err(|e| Aria2Error::FileOpen(format!("path {}: {e}", path.display())))?;
    debug!("Opened file for positioned I/O: {:?}", path);

    // Preserve existing data on resume. Apply the expected size only to a new
    // zero-length file.
    if let Some(size) = total_size {
        let current_size = file.metadata()?.len();
        if current_size == 0 && size > 0 {
            file.set_len(size)?;
            debug!("Pre-allocated file to {} bytes: {:?}", size, path);
        }
    }

    Ok(file)
}

#[async_trait]
impl SeekableDiskWriter for PositionedDiskWriter {
    async fn open(&mut self) -> Result<()> {
        self.ensure_open().await
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.write_bytes_at(offset, bytes::Bytes::copy_from_slice(data))
            .await
    }

    /// Zero-copy write: accepts `Bytes` directly. Since `pwrite` takes `&[u8]`,
    /// we simply dereference the `Bytes` (no copy — `Bytes` derefs to `[u8]`).
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        self.ensure_open().await?;
        let file = self.file_handle()?;
        tokio::task::spawn_blocking(move || write_all_at(file.as_ref(), &data, offset))
            .await
            .map_err(|e| Aria2Error::Io(format!("positioned write task failed: {e}")))?
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.ensure_open().await?;
        if buf.is_empty() {
            return Ok(0);
        }
        let file = self.file_handle()?;
        let len = buf.len();
        let (data, n) = tokio::task::spawn_blocking(move || {
            let mut data = vec![0u8; len];
            let n = read_exact_at(file.as_ref(), &mut data, offset)?;
            Ok::<_, Aria2Error>((data, n))
        })
        .await
        .map_err(|e| Aria2Error::Io(format!("positioned read task failed: {e}")))??;
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        self.ensure_open().await?;
        let file = self.file_handle()?;
        tokio::task::spawn_blocking(move || file.set_len(length))
            .await
            .map_err(|e| Aria2Error::Io(format!("positioned truncate task failed: {e}")))??;
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.file.is_some() {
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
        if let Some(file) = self.file.as_ref().cloned() {
            tokio::task::spawn_blocking(move || file.metadata().map(|metadata| metadata.len()))
                .await
                .map_err(|e| Aria2Error::Io(format!("positioned metadata task failed: {e}")))?
                .map_err(Aria2Error::from)
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
        if let Some(file) = self.file.as_ref().cloned() {
            // Ensure all buffered data reaches stable storage before closing.
            // This is the ONLY place sync_all (fsync) is called — the hot-path
            // flush() intentionally skips it for throughput.
            tokio::task::spawn_blocking(move || file.sync_all())
                .await
                .map_err(|e| Aria2Error::Io(format!("positioned close task failed: {e}")))??;
        }
        self.file = None;
        Ok(())
    }
}
