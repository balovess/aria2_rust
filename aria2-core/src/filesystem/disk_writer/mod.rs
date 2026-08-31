//! Disk writer traits and concrete implementations.
//!
//! This module provides two trait hierarchies:
//! - [`DiskWriter`] — simple sequential write + finalize (used for control files, etc.)
//! - [`SeekableDiskWriter`] — positioned (offset-based) read/write with cache support
//!
//! Concrete implementations:
//! - [`DefaultDiskWriter`] — direct file writer (sequential writes)
//! - [`ByteArrayDiskWriter`] — in-memory byte buffer writer
//! - [`CachedDiskWriter`] — write-back cache layered over positioned I/O

mod atomic;
mod buffered;

#[cfg(test)]
mod tests;

pub use atomic::{ByteArrayDiskWriter, DefaultDiskWriter};
pub use buffered::{CachedDiskWriter, CachedDiskWriterStats};

use crate::error::Result;
use async_trait::async_trait;
use std::path::Path;

/// Build the sequential writer used by single-source download commands.
///
/// Metadata sources selected by `follow-torrent=mem` or
/// `follow-metalink=mem` use the same writer interface as normal downloads,
/// but keep the completed bytes in memory and never open the output path.
/// Keeping this choice at one seam prevents protocol adapters from drifting
/// in their memory-download behavior.
pub fn new_sequential_download_writer(
    path: &Path,
    in_memory: bool,
    write_offset: u64,
    expected_length: Option<u64>,
) -> Box<dyn DiskWriter> {
    if in_memory {
        let capacity = expected_length.unwrap_or_default().min(usize::MAX as u64) as usize;
        Box::new(ByteArrayDiskWriter::with_capacity(capacity))
    } else if write_offset > 0 {
        Box::new(DefaultDiskWriter::new_with_offset(path, write_offset))
    } else {
        Box::new(DefaultDiskWriter::new(path))
    }
}

// ── Sequential writer trait ──────────────────────────────────────────────

#[async_trait]
pub trait DiskWriter: Send + Sync {
    async fn write(&mut self, data: &[u8]) -> Result<()>;
    async fn flush(&mut self) -> Result<()>;
    async fn finalize(&mut self) -> Result<Vec<u8>>;
}

#[async_trait]
impl DiskWriter for Box<dyn DiskWriter> {
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.as_mut().write(data).await
    }

    async fn flush(&mut self) -> Result<()> {
        self.as_mut().flush().await
    }

    async fn finalize(&mut self) -> Result<Vec<u8>> {
        self.as_mut().finalize().await
    }
}

// ── Positioned (seekable) writer trait ───────────────────────────────────

#[async_trait]
#[allow(clippy::len_without_is_empty)]
pub trait SeekableDiskWriter: Send + Sync {
    async fn open(&mut self) -> Result<()>;
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;

    /// Zero-copy write method accepting `bytes::Bytes` directly.
    /// This avoids the intermediate copy when the caller already has Bytes.
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        // Default implementation: delegate to write_at with slice
        self.write_at(offset, &data).await
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize>;
    async fn truncate(&mut self, length: u64) -> Result<()>;
    async fn flush(&mut self) -> Result<()>;
    async fn len(&self) -> Result<u64>;
    fn path(&self) -> &Path;

    /// Close the writer and release underlying file resources.
    ///
    /// Default implementation is a no-op; implementors should override to
    /// truly release file handles and memory mappings. After `close`, the
    /// writer can be reopened with `open()`.
    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl SeekableDiskWriter for Box<dyn SeekableDiskWriter> {
    async fn open(&mut self) -> Result<()> {
        self.as_mut().open().await
    }
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.as_mut().write_at(offset, data).await
    }
    async fn write_bytes_at(&mut self, offset: u64, data: bytes::Bytes) -> Result<()> {
        self.as_mut().write_bytes_at(offset, data).await
    }
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.as_mut().read_at(offset, buf).await
    }
    async fn truncate(&mut self, length: u64) -> Result<()> {
        self.as_mut().truncate(length).await
    }
    async fn flush(&mut self) -> Result<()> {
        self.as_mut().flush().await
    }
    async fn len(&self) -> Result<u64> {
        self.as_ref().len().await
    }
    fn path(&self) -> &std::path::Path {
        self.as_ref().path()
    }
    async fn close(&mut self) -> Result<()> {
        self.as_mut().close().await
    }
}
