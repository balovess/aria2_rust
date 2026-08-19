//! Memory-mapped disk writer using `memmap2::MmapMut` for direct memory access
//! to the file's page cache.
//!
//! # Architecture
//!
//! [`MmapDiskWriter`] uses an [`Inner`] enum to select between two strategies:
//! - [`Inner::Mmap`]: A writable memory mapping (`MmapMut`) backed by the file.
//!   Writes are direct memory copies into the mapped region (no syscalls per
//!   write). Reads are direct memory reads.
//! - [`Inner::Fallback`]: A [`PositionedDiskWriter`] used when mmap creation
//!   fails (e.g., zero-length file, unsupported filesystem, permission error)
//!   or after `truncate` is called (remapping is complex; we switch to
//!   positioned I/O for v1).
//!
//! # Concurrency
//!
//! `MmapDiskWriter` holds `&mut self` for all write operations, so concurrent
//! writes require external synchronization (e.g., `Arc<tokio::sync::Mutex<>>`).
//! This matches the [`SeekableDiskWriter`] trait's `&mut self` requirement.
//! The mmap itself is safe for concurrent reads, but the trait mandates
//! `&mut self` for consistency across implementations.
//!
//! # SIGBUS risk & multi-process constraint
//!
//! **Only one process may hold a writable mapping of the file at a time.**
//! Multi-process resume of the same output file is NOT supported while a
//! mapping is active, and neither is any external `truncate`/`set_len` on a
//! file another process has mapped writable. Both of these scenarios can
//! raise **SIGBUS** (a process-level abort that Rust cannot catch):
//!
//! - **External truncation**: if the file is truncated below a page the
//!   process later writes or reads, the faulting access raises SIGBUS. This
//!   can be triggered by a second aria2 instance resuming/truncating the same
//!   output file.
//! - **Disk full during write-back**: if the filesystem runs out of space
//!   while the kernel writes back a dirty page, the access that dirtied the
//!   page raises SIGBUS. The kernel cannot defer the error to `flush()`.
//!
//! The download engine therefore performs a disk-space pre-check
//! ([`crate::filesystem::disk_space::check_disk_space`]) and pre-allocation
//! (`fallocate`) *before* creating the mmap — the main `file_allocation = mmap`
//! path is covered. Direct users of this writer that bypass the allocation
//! layer are responsible for (1) reserving enough free space and (2) ensuring
//! no other process writes/truncates the same file.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use memmap2::MmapMut;
use tracing::{debug, warn};

use crate::error::{Aria2Error, Result};

use super::disk_writer::SeekableDiskWriter;
use super::positioned_disk_writer::PositionedDiskWriter;

/// Internal writer strategy: memory-mapped or positioned-I/O fallback.
enum Inner {
    /// Memory-mapped mode: both the file handle and the writable mapping are
    /// held. The file must remain open for the mapping to stay valid.
    Mmap {
        /// Underlying file handle. Kept alive to ensure the mmap remains valid;
        /// never read directly (the mmap provides all data access). Dropping
        /// this field would invalidate the mapping.
        #[allow(dead_code)]
        file: std::fs::File,
        /// Writable memory mapping of the file.
        mmap: MmapMut,
    },
    /// Fallback mode: used when mmap creation fails or after `truncate`.
    /// Delegates all operations to a [`PositionedDiskWriter`].
    Fallback(PositionedDiskWriter),
}

/// A disk writer that uses memory-mapped I/O for high-performance reads/writes.
///
/// Falls back to [`PositionedDiskWriter`] (positioned `pwrite`/`seek_write`)
/// when mmap cannot be created (e.g., zero-length file) or after `truncate`
/// is called (remapping after resize is not supported in v1).
///
/// # Example
/// ```ignore
/// use aria2_core::filesystem::mmap_disk_writer::MmapDiskWriter;
/// use aria2_core::filesystem::disk_writer::SeekableDiskWriter;
///
/// # async fn example() -> anyhow::Result<()> {
/// let mut writer = MmapDiskWriter::new(std::path::Path::new("output.bin"), Some(4096));
/// writer.open().await?;
/// writer.write_at(0, b"hello mmap").await?;
/// writer.flush().await?;
/// # Ok(())
/// # }
/// ```
pub struct MmapDiskWriter {
    /// Active writer strategy. `None` when the writer is closed.
    inner: Option<Inner>,
    path: PathBuf,
    total_size: Option<u64>,
    opened: bool,
}

impl MmapDiskWriter {
    /// Create a new `MmapDiskWriter` for the given path.
    ///
    /// If `total_size` is provided and the file is newly created (size 0), the
    /// file is pre-allocated to `total_size` bytes on first open. This is
    /// required for mmap since a zero-length file cannot be mapped.
    pub fn new(path: &Path, total_size: Option<u64>) -> Self {
        Self {
            inner: None,
            path: path.to_path_buf(),
            total_size,
            opened: false,
        }
    }

    /// Create parent directories if they don't exist.
    fn ensure_parent_dirs(&self) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Aria2Error::DirCreate(format!("{}: {e}", parent.display())))?;
            debug!("Created parent directories for {:?}", self.path);
        }
        Ok(())
    }

    /// Open the file, pre-allocate if needed, and try to create an mmap.
    ///
    /// If the file size is 0 (cannot mmap) or `MmapMut::map_mut` fails,
    /// falls back to a [`PositionedDiskWriter`].
    fn open_sync(&mut self) -> Result<()> {
        self.ensure_parent_dirs()?;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false) // Explicit: preserve existing data for resume scenarios.
            .open(&self.path)
            .map_err(|e| Aria2Error::FileOpen(format!("{}: {e}", self.path.display())))?;

        // Pre-allocate if total_size is provided and file is new (size 0).
        if let Some(size) = self.total_size {
            let current_size = file.metadata()?.len();
            if current_size == 0 && size > 0 {
                // Defensive disk-space pre-check before set_len. The download
                // engine's allocation layer (preallocate_file_with_progress /
                // FileAllocationMan) already runs check_disk_space for the
                // main mmap path; this guards direct users who bypass it.
                // Non-fatal by design: log and continue, since set_len still
                // fails cleanly if the filesystem rejects the allocation,
                // whereas running out of space during dirty-page write-back
                // would raise SIGBUS (see module docs).
                if let Err(e) = crate::filesystem::disk_space::check_disk_space(&self.path, size) {
                    warn!(
                        "Insufficient disk space for mmap pre-allocation of {} bytes at {:?}: {}",
                        size, self.path, e
                    );
                }
                file.set_len(size)?;
                debug!("Pre-allocated file to {} bytes: {:?}", size, self.path);
            }
        }

        let file_size = file.metadata()?.len();
        if file_size == 0 {
            // Cannot mmap a zero-length file — use positioned I/O fallback.
            warn!(
                "File size is 0, cannot mmap: {:?}, using positioned I/O fallback",
                self.path
            );
            // Drop our file handle; PositionedDiskWriter will open its own.
            // The fallback writer is stored unopened; the async `open()`
            // method will call `writer.open().await` to finish initialization.
            drop(file);
            let writer = PositionedDiskWriter::new(&self.path, self.total_size);
            self.inner = Some(Inner::Fallback(writer));
            return Ok(());
        }

        // Try to create the memory mapping.
        // SAFETY: The file was opened with read+write access above and is
        // non-empty (checked above). The mapping is valid for the file's
        // current size and is unmapped when the Inner::Mmap variant is
        // dropped.
        //
        // Caller-side invariants required for soundness (see module docs):
        //  * NO OTHER PROCESS may concurrently hold a writable mapping of, or
        //    truncate (`set_len`/`ftruncate`), this file while the mapping is
        //    active. Multi-process resume of the same output file is NOT
        //    supported — external truncation makes accesses past the new EOF
        //    raise SIGBUS (process abort, not catchable in Rust).
        //  * The caller must ensure the backing filesystem has enough free
        //    space for the mapped size. The download engine's allocation
        //    layer runs `check_disk_space` before mapping; direct users must
        //    do the same (see `open_sync` for a defensive warning).
        match unsafe { MmapMut::map_mut(&file) } {
            Ok(mmap) => {
                debug!(
                    "Created mmap for {:?}, size: {} bytes",
                    self.path, file_size
                );
                self.inner = Some(Inner::Mmap { file, mmap });
            }
            Err(e) => {
                warn!(
                    "mmap failed for {:?}: {}, using positioned I/O fallback",
                    self.path, e
                );
                // Drop our file handle; PositionedDiskWriter will open its own.
                // The fallback writer is stored unopened; the async `open()`
                // method will call `writer.open().await` to finish initialization.
                drop(file);
                let writer = PositionedDiskWriter::new(&self.path, self.total_size);
                self.inner = Some(Inner::Fallback(writer));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SeekableDiskWriter for MmapDiskWriter {
    async fn open(&mut self) -> Result<()> {
        if self.opened {
            return Ok(());
        }

        if self.inner.is_none() {
            self.open_sync()?;
        }

        // If we fell back to PositionedDiskWriter, ensure it's opened.
        if let Some(Inner::Fallback(ref mut writer)) = self.inner {
            writer.open().await?;
        }

        self.opened = true;
        Ok(())
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.open().await?;
        match self.inner.as_mut() {
            Some(Inner::Mmap { mmap, .. }) => {
                let start = usize::try_from(offset)
                    .map_err(|_| Aria2Error::Io("write offset exceeds usize range".into()))?;
                let end = start
                    .checked_add(data.len())
                    .ok_or_else(|| Aria2Error::Io("write offset + length overflow".into()))?;
                if end > mmap.len() {
                    return Err(Aria2Error::Io(format!(
                        "write at offset {} len {} exceeds mmap size {}",
                        offset,
                        data.len(),
                        mmap.len()
                    )));
                }
                mmap[start..end].copy_from_slice(data);
                Ok(())
            }
            Some(Inner::Fallback(writer)) => writer.write_at(offset, data).await,
            None => Err(Aria2Error::Io("writer not open".into())),
        }
    }

    /// Write `Bytes` to the mmap region.
    ///
    /// For the mmap variant, a memory copy is unavoidable — `Bytes` is an
    /// `Arc`-backed buffer that cannot be "injected" into the mmap region.
    /// For the fallback variant, this is zero-copy (`pwrite` takes `&data`).
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        self.open().await?;
        match self.inner.as_mut() {
            Some(Inner::Mmap { mmap, .. }) => {
                let start = usize::try_from(offset)
                    .map_err(|_| Aria2Error::Io("write offset exceeds usize range".into()))?;
                let end = start
                    .checked_add(data.len())
                    .ok_or_else(|| Aria2Error::Io("write offset + length overflow".into()))?;
                if end > mmap.len() {
                    return Err(Aria2Error::Io(format!(
                        "write at offset {} len {} exceeds mmap size {}",
                        offset,
                        data.len(),
                        mmap.len()
                    )));
                }
                mmap[start..end].copy_from_slice(&data);
                Ok(())
            }
            Some(Inner::Fallback(writer)) => writer.write_bytes_at(offset, data).await,
            None => Err(Aria2Error::Io("writer not open".into())),
        }
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.open().await?;
        match self.inner.as_mut() {
            Some(Inner::Mmap { mmap, .. }) => {
                let start = usize::try_from(offset)
                    .map_err(|_| Aria2Error::Io("read offset exceeds usize range".into()))?;
                if start >= mmap.len() {
                    return Ok(0); // EOF
                }
                let available = mmap.len() - start;
                let to_read = buf.len().min(available);
                buf[..to_read].copy_from_slice(&mmap[start..start + to_read]);
                Ok(to_read)
            }
            Some(Inner::Fallback(writer)) => writer.read_at(offset, buf).await,
            None => Err(Aria2Error::Io("writer not open".into())),
        }
    }

    async fn truncate(&mut self, length: u64) -> Result<()> {
        self.open().await?;
        match self.inner.as_mut() {
            Some(Inner::Mmap { mmap, .. }) => {
                // Flush dirty pages to the kernel page cache before truncating.
                // This ensures data written via the mmap is visible to the
                // subsequent file operations (set_len, read via positioned I/O).
                // Use flush_async (MS_ASYNC) — the data reaches the page cache
                // immediately, making it visible to file reads. Dropping the
                // mmap below triggers an implicit munmap which also writes back.
                if let Err(e) = mmap.flush_async() {
                    warn!("mmap flush_async before truncate failed: {}", e);
                }
                // Drop the mmap and file, switch to fallback for truncate.
                // v1 does not support remapping after resize.
                self.inner = None;
                let mut writer = PositionedDiskWriter::new(&self.path, self.total_size);
                writer.open().await?;
                writer.truncate(length).await?;
                self.inner = Some(Inner::Fallback(writer));
                debug!(
                    "Truncated mmap writer to {} bytes, switched to fallback: {:?}",
                    length, self.path
                );
                Ok(())
            }
            Some(Inner::Fallback(writer)) => writer.truncate(length).await,
            None => Err(Aria2Error::Io("writer not open".into())),
        }
    }

    async fn flush(&mut self) -> Result<()> {
        match self.inner.as_mut() {
            Some(Inner::Mmap { mmap, .. }) => {
                // Flush dirty pages to the kernel page cache using MS_ASYNC.
                // We intentionally do NOT use MS_SYNC (synchronous flush to
                // disk via mmap.flush()) — it is too expensive for normal
                // flush operations. MS_ASYNC makes data visible in the page
                // cache immediately; the OS writes back to stable storage
                // asynchronously. Data is visible to other file readers.
                mmap.flush_async()
                    .map_err(|e| Aria2Error::Io(format!("mmap flush_async failed: {}", e)))?;
                Ok(())
            }
            Some(Inner::Fallback(writer)) => writer.flush().await,
            None => Ok(()), // No-op if not open
        }
    }

    async fn len(&self) -> Result<u64> {
        match &self.inner {
            Some(Inner::Mmap { mmap, .. }) => Ok(mmap.len() as u64),
            Some(Inner::Fallback(writer)) => writer.len().await,
            None => {
                if let Some(size) = self.total_size {
                    Ok(size)
                } else {
                    Ok(0)
                }
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Close the writer, releasing the file handle and memory mapping.
    ///
    /// After close, the writer can be reopened with `open()`.
    async fn close(&mut self) -> Result<()> {
        self.flush().await?;
        // Drop the file and mmap (or the fallback writer), releasing resources.
        self.inner = None;
        self.opened = false;
        Ok(())
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
    async fn test_mmap_writer_basic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_basic.bin");

        let mut writer = MmapDiskWriter::new(&path, Some(1024));
        writer.open().await.unwrap();
        writer.write_at(0, b"hello mmap").await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 10];
        let n = writer.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 10);
        assert_eq!(&buf, b"hello mmap");
    }

    #[tokio::test]
    async fn test_mmap_writer_write_at_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_offset.bin");

        let mut writer = MmapDiskWriter::new(&path, Some(512));
        writer.open().await.unwrap();

        // Write at non-zero offset
        writer.write_at(100, b"offset data").await.unwrap();
        writer.flush().await.unwrap();

        // Read back at offset 100
        let mut buf = [0u8; 11];
        let n = writer.read_at(100, &mut buf).await.unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf, b"offset data");

        // Verify offset 0 is zero-filled (mmap initializes to zeros)
        let mut buf0 = [0xFFu8; 16];
        let n0 = writer.read_at(0, &mut buf0).await.unwrap();
        assert_eq!(n0, 16, "should read full 16 bytes from zero-filled region");
        assert!(
            buf0.iter().all(|&b| b == 0),
            "offset 0 should be zero-filled, got {:?}",
            buf0
        );
    }

    #[tokio::test]
    async fn test_mmap_writer_concurrent_writes() {
        // MmapDiskWriter holds &mut self, so concurrent writes require external
        // sync. This test verifies data integrity with sequential writes to
        // non-overlapping offsets through an Arc<tokio::sync::Mutex<>>.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_concurrent.bin");

        let chunk_size: usize = 64 * 1024;
        let num_tasks: usize = 4;
        let total_size = (chunk_size * num_tasks) as u64;

        let mut writer = MmapDiskWriter::new(&path, Some(total_size));
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

        // Verify data integrity by reading back through the writer
        for i in 0..num_tasks {
            let offset = (i as u64) * chunk_size as u64;
            let expected = (i as u8) + 1;
            let mut buf = vec![0u8; chunk_size];
            let mut guard = writer.lock().await;
            let n = guard.read_at(offset, &mut buf).await.unwrap();
            assert_eq!(n, chunk_size, "read length mismatch at chunk {}", i);
            assert!(
                buf.iter().all(|&b| b == expected),
                "data mismatch in chunk {}",
                i
            );
        }
    }

    #[tokio::test]
    async fn test_mmap_writer_fallback_on_open_failure() {
        // Test the fallback path by constructing a writer with total_size=None
        // on a new (zero-length) file. mmap cannot map a zero-length file,
        // so the writer should fall back to PositionedDiskWriter.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_fallback.bin");

        let mut writer = MmapDiskWriter::new(&path, None);
        writer.open().await.unwrap();

        // Verify writes work (via fallback PositionedDiskWriter)
        writer.write_at(0, b"fallback works").await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 14];
        let n = writer.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 14);
        assert_eq!(&buf, b"fallback works");

        // Verify the inner is Fallback (indirectly: truncate should work
        // without switching modes, since we're already in fallback)
        writer.truncate(7).await.unwrap();
        let len = writer.len().await.unwrap();
        assert_eq!(len, 7);
    }

    #[tokio::test]
    async fn test_mmap_writer_truncate_and_len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_trunc.bin");

        let mut writer = MmapDiskWriter::new(&path, Some(2048));
        writer.open().await.unwrap();

        // Initially allocated to total_size
        let len = writer.len().await.unwrap();
        assert_eq!(len, 2048);

        // Write some data
        writer.write_at(0, b"before truncate").await.unwrap();
        writer.flush().await.unwrap();

        // Truncate to a smaller size — switches to fallback mode
        writer.truncate(512).await.unwrap();
        let len = writer.len().await.unwrap();
        assert_eq!(len, 512);

        // Verify data before the truncation point is preserved
        let mut buf = [0u8; 15];
        let n = writer.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 15);
        assert_eq!(&buf, b"before truncate");
    }

    #[tokio::test]
    async fn test_mmap_writer_write_bytes_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_bytes.bin");

        let mut writer = MmapDiskWriter::new(&path, Some(256));
        writer.open().await.unwrap();

        let data = bytes::Bytes::from(vec![0xAB; 128]);
        writer.write_bytes_at(0, data).await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 128];
        let n = writer.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 128);
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    #[tokio::test]
    async fn test_mmap_writer_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_close.bin");

        let mut writer = MmapDiskWriter::new(&path, Some(1024));
        writer.open().await.unwrap();
        writer.write_at(0, b"before close").await.unwrap();
        writer.close().await.unwrap();
        assert!(!writer.opened);

        writer.open().await.unwrap();
        writer.write_at(12, b" after reopen").await.unwrap();
        writer.close().await.unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_eq!(&content[..25], b"before close after reopen");
    }

    #[tokio::test]
    async fn test_mmap_writer_len_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_len_before.bin");

        let writer = MmapDiskWriter::new(&path, Some(9999));
        let len = writer.len().await.unwrap();
        assert_eq!(len, 9999, "should return total_size before open");
    }

    #[tokio::test]
    async fn test_mmap_writer_resume_does_not_truncate() {
        // Verify that opening an existing file with total_size does NOT
        // truncate existing data — critical for resume scenarios.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mmap_resume.bin");

        // First writer: create and write data
        {
            let mut w = MmapDiskWriter::new(&path, Some(1024));
            w.open().await.unwrap();
            w.write_at(0, b"resume-data").await.unwrap();
            w.flush().await.unwrap();
        }

        // Second writer: open existing file with same total_size
        {
            let mut w = MmapDiskWriter::new(&path, Some(1024));
            w.open().await.unwrap();
            let mut buf = [0u8; 11];
            let n = w.read_at(0, &mut buf).await.unwrap();
            assert_eq!(n, 11);
            assert_eq!(&buf, b"resume-data", "existing data must survive reopen");
        }
    }
}
