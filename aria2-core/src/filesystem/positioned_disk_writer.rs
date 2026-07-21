//! Positioned disk writer using OS-native `pwrite`/`seek_write` for concurrent
//! writes to non-overlapping offsets without a global async mutex.
//!
//! Platform support:
//! - Unix: [`std::os::unix::fs::FileExt::write_at`] (wraps `pwrite(2)`)
//! - Windows: [`std::os::windows::fs::FileExt::seek_write`]
//!
//! # Concurrency model
//!
//! The underlying file handle is wrapped in a [`std::sync::Mutex`] held ONLY
//! for the brief duration of each synchronous `pwrite`/`seek_write` syscall.
//! This is fundamentally different from the legacy
//! `Arc<tokio::sync::Mutex<DirectDiskAdaptor>>` design which held the lock
//! across async await points and serialized all writes.
//!
//! Here the lock is held only for the synchronous syscall (microseconds),
//! never across `.await` points. When multiple [`PositionedDiskWriter`]
//! instances reference the same file path (each opening its own file
//! descriptor), non-overlapping writes execute concurrently at the OS level
//! because `pwrite` is atomic and offset-based — it does not mutate the
//! shared file cursor.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use tracing::debug;

use crate::error::{Aria2Error, Result};

use super::disk_writer::SeekableDiskWriter;

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

// =========================================================================
// Platform-specific positioned I/O helpers
// =========================================================================

/// Positioned write that loops to handle partial writes.
///
/// Writes the entire `buf` at `offset` without modifying the file cursor,
/// preserving `pwrite(2)` semantics while guaranteeing a complete write.
fn write_all_at(file: &std::fs::File, mut buf: &[u8], mut offset: u64) -> Result<()> {
    while !buf.is_empty() {
        let n = positioned_write(file, buf, offset)?;
        if n == 0 {
            return Err(Aria2Error::Io(
                "positioned write returned 0 — failed to write whole buffer".into(),
            ));
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

/// Positioned read that loops to fill as much of `buf` as possible.
///
/// Returns the number of bytes read (may be less than `buf.len()` at EOF).
fn read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
    let mut filled = 0usize;
    let mut current_offset = offset;
    while filled < buf.len() {
        let n = positioned_read(file, &mut buf[filled..], current_offset)?;
        if n == 0 {
            break; // EOF reached
        }
        filled += n;
        current_offset += n as u64;
    }
    Ok(filled)
}

/// Single positioned write syscall. Returns bytes written.
#[cfg(unix)]
fn positioned_write(file: &std::fs::File, buf: &[u8], offset: u64) -> Result<usize> {
    use std::os::unix::fs::FileExt;
    Ok(file.write_at(buf, offset)?)
}

/// Single positioned read syscall. Returns bytes read.
#[cfg(unix)]
fn positioned_read(file: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
    use std::os::unix::fs::FileExt;
    Ok(file.read_at(buf, offset)?)
}

/// Single positioned write syscall (Windows). Returns bytes written.
#[cfg(windows)]
fn positioned_write(file: &std::fs::File, buf: &[u8], offset: u64) -> Result<usize> {
    use std::os::windows::fs::FileExt;
    Ok(file.seek_write(buf, offset)?)
}

/// Single positioned read syscall (Windows). Returns bytes read.
#[cfg(windows)]
fn positioned_read(file: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
    use std::os::windows::fs::FileExt;
    Ok(file.seek_read(buf, offset)?)
}

#[cfg(not(any(unix, windows)))]
fn positioned_write(_file: &std::fs::File, _buf: &[u8], _offset: u64) -> Result<usize> {
    Err(Aria2Error::Io(
        "positioned write not supported on this platform".into(),
    ))
}

#[cfg(not(any(unix, windows)))]
fn positioned_read(_file: &std::fs::File, _buf: &mut [u8], _offset: u64) -> Result<usize> {
    Err(Aria2Error::Io(
        "positioned read not supported on this platform".into(),
    ))
}

/// Create the best available positioned writer for the current platform.
///
/// On Linux with the `io_uring` feature enabled, returns an [`IoUringDiskWriter`]
/// that uses the io_uring syscall interface for async positioned I/O. On all
/// other platforms (or without the feature), returns a [`PositionedDiskWriter`]
/// that uses synchronous `pwrite`/`seek_write`.
///
/// # Runtime requirement (io_uring)
///
/// When the `io_uring` feature is enabled on Linux, the returned writer MUST be
/// driven from within a `tokio_uring` runtime context (e.g. inside
/// `tokio_uring::start`). Using it inside a regular `tokio` runtime will panic
/// because `tokio_uring::fs` operations require the io_uring reactor.
///
/// This factory is intentionally NOT wired into the default download pipeline.
/// The main pipeline uses [`PositionedDiskWriter`] directly via `CachedDiskWriter`.
pub fn create_positioned_writer(
    path: &Path,
    total_size: Option<u64>,
) -> Box<dyn SeekableDiskWriter> {
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    {
        Box::new(IoUringDiskWriter::new(path, total_size))
    }
    #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
    {
        Box::new(PositionedDiskWriter::new(path, total_size))
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_positioned_write_basic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_basic.bin");

        let mut writer = PositionedDiskWriter::new(&path, Some(1024));
        writer.open().await.unwrap();
        writer.write_at(0, b"hello world").await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 11];
        let n = writer.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf, b"hello world");
    }

    #[tokio::test]
    async fn test_positioned_write_at_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_offset.bin");

        let mut writer = PositionedDiskWriter::new(&path, None);
        writer.open().await.unwrap();
        // "data at 100" is 11 bytes
        writer.write_at(100, b"data at 100").await.unwrap();
        writer.flush().await.unwrap();

        // Read back at offset 100
        let mut buf = [0u8; 11];
        let n = writer.read_at(100, &mut buf).await.unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf, b"data at 100");

        // Verify offset 0 is zero-filled (sparse hole / OS zero-fill on extend)
        let mut buf0 = [0xFFu8; 12];
        let n0 = writer.read_at(0, &mut buf0).await.unwrap();
        assert_eq!(n0, 12, "should read full 12 bytes from zero-filled region");
        assert!(
            buf0.iter().all(|&b| b == 0),
            "offset 0 should be zero-filled, got {:?}",
            buf0
        );
    }

    #[tokio::test]
    async fn test_positioned_writer_truncate_and_len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_trunc.bin");

        let mut writer = PositionedDiskWriter::new(&path, Some(2048));
        writer.open().await.unwrap();

        // Pre-allocated to total_size
        let len = writer.len().await.unwrap();
        assert_eq!(len, 2048);

        writer.truncate(512).await.unwrap();
        let len = writer.len().await.unwrap();
        assert_eq!(len, 512);
    }

    #[tokio::test]
    async fn test_positioned_writer_len_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_len_before_open.bin");

        let writer = PositionedDiskWriter::new(&path, Some(9999));
        let len = writer.len().await.unwrap();
        assert_eq!(len, 9999, "should return total_size before open");
    }

    #[tokio::test]
    async fn test_positioned_writer_len_no_total_size_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_len_none.bin");

        let writer = PositionedDiskWriter::new(&path, None);
        let len = writer.len().await.unwrap();
        assert_eq!(len, 0, "should return 0 before open when no total_size");
    }

    #[tokio::test]
    async fn test_positioned_writer_resume_does_not_truncate() {
        // Verify that opening an existing file with total_size does NOT truncate
        // existing data — critical for resume scenarios.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_resume.bin");

        // First writer: create and write data
        {
            let mut w = PositionedDiskWriter::new(&path, Some(1024));
            w.open().await.unwrap();
            w.write_at(0, b"resume-data").await.unwrap();
            w.flush().await.unwrap();
        }

        // Second writer: open existing file with same total_size
        {
            let mut w = PositionedDiskWriter::new(&path, Some(1024));
            w.open().await.unwrap();
            let mut buf = [0u8; 11];
            let n = w.read_at(0, &mut buf).await.unwrap();
            assert_eq!(n, 11);
            assert_eq!(&buf, b"resume-data", "existing data must survive reopen");
        }
    }

    #[tokio::test]
    async fn test_positioned_writer_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("file.bin");

        let mut writer = PositionedDiskWriter::new(&path, Some(64));
        writer.open().await.unwrap();
        writer.write_at(0, b"x").await.unwrap();
        writer.flush().await.unwrap();

        assert!(path.exists(), "file should be created with parent dirs");
    }

    #[tokio::test]
    async fn test_concurrent_writes_non_overlapping() {
        // Test with a shared writer wrapped in Arc<tokio::sync::Mutex<>>.
        // The internal std::sync::Mutex is held only for the syscall
        // (microseconds), so even with the outer serialization the test
        // validates positioned-write correctness and data integrity.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_concurrent_shared.bin");

        let chunk_size: usize = 64 * 1024;
        let num_tasks: usize = 4;

        let mut writer = PositionedDiskWriter::new(&path, Some((chunk_size * num_tasks) as u64));
        writer.open().await.unwrap();
        let writer = Arc::new(tokio::sync::Mutex::new(writer));

        let mut handles = Vec::with_capacity(num_tasks);
        for i in 0..num_tasks {
            let offset = (i as u64) * chunk_size as u64;
            let fill = (i as u8) + 1;
            let data = bytes::Bytes::from(vec![fill; chunk_size]);
            let w = writer.clone();
            handles.push(tokio::spawn(async move {
                let mut guard = w.lock().await;
                guard.write_bytes_at(offset, data).await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        {
            let mut guard = writer.lock().await;
            guard.flush().await.unwrap();
        }

        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(content.len(), chunk_size * num_tasks);
        for i in 0..num_tasks {
            let start = i * chunk_size;
            let expected = (i as u8) + 1;
            let chunk = &content[start..start + chunk_size];
            assert!(
                chunk.iter().all(|&b| b == expected),
                "data mismatch in task {} chunk",
                i
            );
        }
    }

    #[tokio::test]
    async fn test_concurrent_writes_separate_writers() {
        // True OS-level concurrency: each task opens its OWN writer to the SAME
        // file path and writes to non-overlapping offsets. pwrite is atomic and
        // offset-based, so concurrent non-overlapping writes are safe.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_concurrent_sep.bin");

        let chunk_size: usize = 64 * 1024;
        let num_tasks: usize = 4;

        // Pre-create and allocate the file via one writer, then drop it.
        {
            let mut w0 = PositionedDiskWriter::new(&path, Some((chunk_size * num_tasks) as u64));
            w0.open().await.unwrap();
            w0.flush().await.unwrap();
        }

        let mut handles = Vec::with_capacity(num_tasks);
        for i in 0..num_tasks {
            let offset = (i as u64) * chunk_size as u64;
            let fill = (i as u8) + 1;
            let data = vec![fill; chunk_size];
            let path_clone = path.clone();
            handles.push(tokio::spawn(async move {
                let mut w = PositionedDiskWriter::new(&path_clone, None);
                w.open().await.unwrap();
                w.write_at(offset, &data).await.unwrap();
                w.flush().await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(content.len(), chunk_size * num_tasks);
        for i in 0..num_tasks {
            let start = i * chunk_size;
            let expected = (i as u8) + 1;
            let chunk = &content[start..start + chunk_size];
            assert!(
                chunk.iter().all(|&b| b == expected),
                "data mismatch in separate-writer task {} chunk",
                i
            );
        }
    }

    #[tokio::test]
    async fn test_positioned_writer_write_bytes_at_zero_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_zero_copy.bin");

        let mut writer = PositionedDiskWriter::new(&path, Some(256));
        writer.open().await.unwrap();

        let data = bytes::Bytes::from(vec![0xAB; 128]);
        writer.write_bytes_at(0, data).await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 128];
        let n = writer.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 128);
        assert!(buf.iter().all(|&b| b == 0xAB));
    }
}

// =========================================================================
// io_uring backend (Linux only, feature-gated)
//
// IMPORTANT: `tokio-uring` requires its own single-threaded runtime
// (`tokio_uring::start` / `tokio_uring::Runtime`). All async operations on
// `IoUringDiskWriter` MUST be driven from within a `tokio_uring` runtime
// context. Using them inside a regular `tokio` runtime will panic at the
// first I/O call because the io_uring reactor is not installed.
//
// This backend is intentionally NOT wired into the default download pipeline
// (which uses `PositionedDiskWriter` via `CachedDiskWriter`). It is an opt-in
// experimental path selected via [`create_positioned_writer`] when the
// `io_uring` feature is enabled on Linux.
// =========================================================================

#[cfg(all(target_os = "linux", feature = "io_uring"))]
mod io_uring_backend {
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
}

// Re-export the io_uring writer at the module level when the feature is on.
#[cfg(all(target_os = "linux", feature = "io_uring"))]
pub use io_uring_backend::IoUringDiskWriter;

// =========================================================================
// io_uring tests (Linux + feature only)
//
// These tests are excluded on Windows/macOS and when the `io_uring` feature is
// off. They use `tokio_uring::start` to drive the io_uring runtime.
// =========================================================================

#[cfg(all(test, target_os = "linux", feature = "io_uring"))]
mod io_uring_tests {
    use super::IoUringDiskWriter;

    #[test]
    fn test_iouring_basic_write_read() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("iouring_basic.bin");

            let mut writer = IoUringDiskWriter::new(&path, Some(1024));
            writer.open().await.unwrap();
            writer.write_at(0, b"hello io_uring").await.unwrap();
            writer.flush().await.unwrap();

            let mut buf = [0u8; 14];
            let n = writer.read_at(0, &mut buf).await.unwrap();
            assert_eq!(n, 14);
            assert_eq!(&buf, b"hello io_uring");
        });
    }

    #[test]
    fn test_iouring_write_at_offset() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("iouring_offset.bin");

            let mut writer = IoUringDiskWriter::new(&path, None);
            writer.open().await.unwrap();
            writer.write_at(100, b"offset data").await.unwrap();
            writer.flush().await.unwrap();

            let mut buf = [0u8; 11];
            let n = writer.read_at(100, &mut buf).await.unwrap();
            assert_eq!(n, 11);
            assert_eq!(&buf, b"offset data");
        });
    }

    #[test]
    fn test_iouring_truncate_and_len() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("iouring_trunc.bin");

            let mut writer = IoUringDiskWriter::new(&path, Some(2048));
            writer.open().await.unwrap();

            let len = writer.len().await.unwrap();
            assert_eq!(len, 2048);

            writer.truncate(512).await.unwrap();
            let len = writer.len().await.unwrap();
            assert_eq!(len, 512);
        });
    }

    #[test]
    fn test_iouring_close_reopen() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("iouring_close.bin");

            let mut writer = IoUringDiskWriter::new(&path, None);
            writer.open().await.unwrap();
            writer.write_at(0, b"before close").await.unwrap();
            writer.flush().await.unwrap();
            writer.close().await.unwrap();

            writer.open().await.unwrap();
            writer.write_at(12, b" after reopen").await.unwrap();
            writer.flush().await.unwrap();
            writer.close().await.unwrap();

            let content = std::fs::read(&path).unwrap();
            assert_eq!(&content, b"before close after reopen");
        });
    }

    #[test]
    fn test_iouring_resume_does_not_truncate() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("iouring_resume.bin");

            // First writer: create and write data
            {
                let mut w = IoUringDiskWriter::new(&path, Some(1024));
                w.open().await.unwrap();
                w.write_at(0, b"resume-data").await.unwrap();
                w.flush().await.unwrap();
                w.close().await.unwrap();
            }

            // Second writer: open existing file with same total_size
            {
                let mut w = IoUringDiskWriter::new(&path, Some(1024));
                w.open().await.unwrap();
                let mut buf = [0u8; 11];
                let n = w.read_at(0, &mut buf).await.unwrap();
                assert_eq!(n, 11);
                assert_eq!(&buf, b"resume-data", "existing data must survive reopen");
            }
        });
    }

    #[test]
    fn test_iouring_creates_parent_dirs() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nested").join("deep").join("file.bin");

            let mut writer = IoUringDiskWriter::new(&path, Some(64));
            writer.open().await.unwrap();
            writer.write_at(0, b"x").await.unwrap();
            writer.flush().await.unwrap();

            assert!(path.exists(), "file should be created with parent dirs");
        });
    }

    #[test]
    fn test_iouring_write_bytes_at() {
        tokio_uring::start(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("iouring_bytes.bin");

            let mut writer = IoUringDiskWriter::new(&path, Some(256));
            writer.open().await.unwrap();

            let data = bytes::Bytes::from(vec![0xAB; 128]);
            writer.write_bytes_at(0, data).await.unwrap();
            writer.flush().await.unwrap();

            let mut buf = [0u8; 128];
            let n = writer.read_at(0, &mut buf).await.unwrap();
            assert_eq!(n, 128);
            assert!(buf.iter().all(|&b| b == 0xAB));
        });
    }
}
