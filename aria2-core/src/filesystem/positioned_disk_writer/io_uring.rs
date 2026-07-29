//! io_uring backend (Linux only, feature-gated)
//!
//! IMPORTANT: `tokio-uring` requires its own single-threaded runtime
//! (`tokio_uring::start` / `tokio_uring::Runtime`). All async operations on
//! `IoUringDiskWriter` MUST be driven from within a `tokio_uring` runtime
//! context. Using them inside a regular `tokio` runtime will panic at the
//! first I/O call because the io_uring reactor is not installed.
//!
//! This backend is intentionally NOT wired into the default download pipeline
//! (which uses `PositionedDiskWriter` via `CachedDiskWriter`). It is an opt-in
//! experimental path selected via [`super::create_positioned_writer`] when the
//! `io_uring` feature is enabled on Linux.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::debug;

use crate::error::{Aria2Error, Result};

use super::SeekableDiskWriter;

/// A disk writer that performs positioned I/O via the Linux `io_uring`
/// syscall interface using [`tokio_uring::fs::File`].
///
/// # Runtime requirement
///
/// All async methods MUST be called from within a `tokio_uring` runtime
/// context (e.g. inside `tokio_uring::start`). The underlying
/// `tokio_uring::fs::File` operations register buffers and completion
/// entries with the io_uring instance, which is only available inside a
/// `tokio_uring` runtime.
///
/// # Concurrency model
///
/// Like [`super::PositionedDiskWriter`], `write_at` takes `&mut self`, so
/// calls on a single instance are serialized at the application level.
/// True OS-level concurrency is achieved by opening separate writer
/// instances (each with its own file descriptor) to the same file path and
/// writing to non-overlapping offsets — `io_uring` submits these as
/// independent SQEs that the kernel can complete in parallel.
pub struct IoUringDiskWriter {
    /// The `tokio_uring::fs::File`, wrapped in `Option` for lazy open and
    /// clean close/reopen semantics.
    file: Option<tokio_uring::fs::File>,
    path: PathBuf,
    total_size: Option<u64>,
}

impl IoUringDiskWriter {
    /// Create a new `IoUringDiskWriter` for the given path.
    ///
    /// If `total_size` is provided and the file is newly created (size 0),
    /// the file is pre-allocated to `total_size` bytes on first open. This
    /// matches [`super::PositionedDiskWriter`] semantics for fragmentation
    /// avoidance and concurrent-write readiness.
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
}

#[async_trait]
impl SeekableDiskWriter for IoUringDiskWriter {
    async fn open(&mut self) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }

        // Create parent directories if needed (resume scenario may have
        // missing dirs). This is a synchronous call but it is fast and
        // only runs once per open.
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
            debug!("Created parent directories for {:?}", self.path);
        }

        // Pre-allocate the file if a total size was specified and the file
        // is newly created (current size 0). `tokio_uring::fs::File` does
        // not expose `set_len`, so we open a transient `std::fs::File`,
        // call `set_len`, and drop it before opening via tokio_uring.
        //
        // Resume safety: we do NOT truncate existing data — only extend
        // the file if it is empty.
        if let Some(size) = self.total_size {
            let std_file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .read(true)
                .open(&self.path)?;
            let current_size = std_file.metadata()?.len();
            if current_size == 0 && size > 0 {
                std_file.set_len(size)?;
                debug!("Pre-allocated file to {} bytes: {:?}", size, self.path);
            }
            drop(std_file);
        }

        // Open via tokio_uring with read+write+create (no truncate) to
        // preserve any existing data for resume scenarios.
        let file = tokio_uring::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&self.path)
            .await
            .map_err(|e| Aria2Error::Io(format!("io_uring open failed: {e}")))?;
        debug!("Opened file for io_uring positioned I/O: {:?}", self.path);

        self.file = Some(file);
        Ok(())
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| Aria2Error::Io("io_uring file not open".into()))?;
        write_all_at_uring(file, data, offset).await
    }

    /// Zero-copy write: accepts `Bytes` directly. Since io_uring `write_at`
    /// takes a buffer reference, we simply dereference the `Bytes` (no copy
    /// — `Bytes` derefs to `[u8]`).
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        self.write_at(offset, &data).await
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| Aria2Error::Io("io_uring file not open".into()))?;
        read_exact_at_uring(file, buf, offset).await
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        // `tokio_uring::fs::File` does not expose `set_len`. Close the
        // io_uring file, truncate via `std::fs::File::set_len`, then reopen
        // via tokio_uring to get a fresh file handle.
        if let Some(file) = self.file.take() {
            let _ = file.close().await.map_err(|e| {
                Aria2Error::Io(format!("io_uring close during truncate failed: {e}"))
            })?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)?
            .set_len(length)?;
        let file = tokio_uring::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&self.path)
            .await
            .map_err(|e| {
                Aria2Error::Io(format!("io_uring reopen after truncate failed: {e}"))
            })?;
        self.file = Some(file);
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if let Some(ref file) = self.file {
            file.sync_all()
                .await
                .map_err(|e| Aria2Error::Io(format!("io_uring sync_all failed: {e}")))?;
        }
        Ok(())
    }

    async fn len(&self) -> Result<u64> {
        if self.file.is_some() {
            // Use a synchronous `stat` (fast, non-blocking syscall) to get
            // the file size. This avoids requiring a tokio runtime context
            // (which may not be available inside tokio_uring::start).
            Ok(std::fs::metadata(&self.path)
                .map(|m| m.len())
                .unwrap_or_else(|_| self.total_size.unwrap_or(0)))
        } else if let Some(size) = self.total_size {
            Ok(size)
        } else {
            Ok(0)
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Close the writer and release the io_uring file handle.
    async fn close(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.close()
                .await
                .map_err(|e| Aria2Error::Io(format!("io_uring close failed: {e}")))?;
        }
        Ok(())
    }
}

/// Positioned write via io_uring that loops to handle partial writes.
///
/// Writes the entire `buf` at `offset`, guaranteeing a complete write.
/// `tokio_uring::fs::File::write_at` returns `(io::Result<usize>, B)` where
/// `B` is the original buffer (for `&[u8]` this is a `Copy` reference, so
/// looping is zero-allocation).
async fn write_all_at_uring(
    file: &tokio_uring::fs::File,
    mut buf: &[u8],
    mut offset: u64,
) -> Result<()> {
    while !buf.is_empty() {
        let (res, _) = file.write_at(buf, offset).await;
        let n = res.map_err(|e| Aria2Error::Io(format!("io_uring write_at failed: {e}")))?;
        if n == 0 {
            return Err(Aria2Error::Io(
                "io_uring write_at returned 0 — failed to write whole buffer".into(),
            ));
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

/// Positioned read via io_uring that loops to fill as much of `buf` as
/// possible. Returns the number of bytes read (may be less than
/// `buf.len()` at EOF).
async fn read_exact_at_uring(
    file: &tokio_uring::fs::File,
    mut buf: &mut [u8],
    mut offset: u64,
) -> Result<usize> {
    let mut filled = 0usize;
    while !buf.is_empty() {
        let (res, returned_buf) = file.read_at(buf, offset).await;
        let n = res.map_err(|e| Aria2Error::Io(format!("io_uring read_at failed: {e}")))?;
        if n == 0 {
            break; // EOF reached
        }
        filled += n;
        offset += n as u64;
        buf = &mut returned_buf[n..];
    }
    Ok(filled)
}