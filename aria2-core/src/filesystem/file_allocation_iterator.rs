//! File allocation iterator trait and single-file implementations.
//!
//! This module provides an incremental, chunked file allocation API that
//! mirrors the C++ aria2 `FileAllocationIterator` hierarchy:
//!
//! - [`FileAllocationIterator`] — async trait for chunked allocation with
//!   progress reporting
//! - [`SingleFileAllocationIterator`] — zero-fill in 256 KiB chunks
//!   (C++ `SingleFileAllocationIterator`)
//! - [`FallocFileAllocationIterator`] — one-shot `fallocate`/`F_PREALLOCATE`
//!   (C++ `FallocFileAllocationIterator`)
//! - [`TruncFileAllocationIterator`] — `ftruncate`/`SetEndOfFile`
//!   (C++ `TruncFileAllocationIterator`)
//! - [`AdaptiveFileAllocationIterator`] — try `fallocate`, fall back to
//!   zero-fill (C++ `AdaptiveFileAllocationIterator`, default for
//!   `--file-allocation=prealloc`)

use async_trait::async_trait;

use crate::error::Result;
use crate::filesystem::disk_adaptor::DiskAdaptor;

/// Chunk size for single-file zero-fill allocation.
/// Matches the C++ constant `BUFSIZE = 256_k` (256 KiB).
const BUF_SIZE: usize = 256 * 1024;

/// Async trait for incremental file allocation.
///
/// Callers invoke [`allocate_chunk`] repeatedly until [`finished`] returns
/// `true`. After each call, [`current_length`] and [`total_length`] report
/// progress. This mirrors the C++ `FileAllocationIterator` interface.
///
/// # C++ Reference
///
/// - `FileAllocationIterator.h` — pure virtual base
/// - `allocateChunk()`, `finished()`, `getCurrentLength()`, `getTotalLength()`
#[async_trait]
pub trait FileAllocationIterator: Send {
    /// Allocate one chunk of disk space.
    ///
    /// On return, [`current_length`] will have advanced by at most one chunk
    /// worth of bytes.
    async fn allocate_chunk(&mut self) -> Result<()>;

    /// Whether allocation is complete.
    fn finished(&self) -> bool;

    /// Bytes allocated so far.
    fn current_length(&self) -> u64;

    /// Total bytes to allocate.
    fn total_length(&self) -> u64;
}

// =========================================================================
// SingleFileAllocationIterator
// =========================================================================

/// Zero-fill allocation iterator that writes 256 KiB chunks of zeros.
///
/// This is the Rust equivalent of C++ `SingleFileAllocationIterator`.
/// It writes aligned zero buffers at increasing offsets. If the last
/// write overshoots the target length, the file is truncated back to
/// the exact target length.
///
/// This iterator is the fallback when `fallocate` is not available or
/// not supported by the filesystem.
pub struct SingleFileAllocationIterator<D: DiskAdaptor> {
    adaptor: D,
    offset: u64,
    total_length: u64,
    /// Zero-fill buffer (reused across chunks). Allocated on first use.
    buffer: Option<Vec<u8>>,
}

impl<D: DiskAdaptor + Default> SingleFileAllocationIterator<D> {
    /// Create a new single-file allocation iterator.
    ///
    /// # Arguments
    /// * `adaptor` — disk adaptor for write operations (must have an open file)
    /// * `offset` — starting offset (current file size, typically 0)
    /// * `total_length` — target file size
    pub fn new(adaptor: D, offset: u64, total_length: u64) -> Self {
        Self {
            adaptor,
            offset,
            total_length,
            buffer: None,
        }
    }

    /// Initialize the zero-fill buffer. Must be called before
    /// `allocate_chunk()`. This is the Rust equivalent of the C++ `init()`
    /// method which allocates the aligned buffer.
    pub fn init(&mut self) {
        if self.buffer.is_none() {
            self.buffer = Some(vec![0u8; BUF_SIZE]);
        }
    }

    /// Get a mutable reference to the underlying adaptor.
    pub fn adaptor_mut(&mut self) -> &mut D {
        &mut self.adaptor
    }
}

#[async_trait]
impl<D: DiskAdaptor + Default> FileAllocationIterator for SingleFileAllocationIterator<D> {
    async fn allocate_chunk(&mut self) -> Result<()> {
        let buf = self.buffer.as_deref().unwrap_or_else(|| {
            static EMPTY: &[u8] = &[0u8; BUF_SIZE];
            EMPTY
        });

        self.adaptor.write(self.offset, buf).await?;
        self.offset += BUF_SIZE as u64;

        // If we wrote past the target length, truncate back.
        if self.total_length < self.offset {
            self.adaptor.truncate(self.total_length).await?;
            self.offset = self.total_length;
        }

        Ok(())
    }

    fn finished(&self) -> bool {
        self.offset >= self.total_length
    }

    fn current_length(&self) -> u64 {
        self.offset.min(self.total_length)
    }

    fn total_length(&self) -> u64 {
        self.total_length
    }
}

// =========================================================================
// FallocFileAllocationIterator
// =========================================================================

/// Fast allocation iterator using `fallocate` / `F_PREALLOCATE` /
/// `SetFileValidData`.
///
/// This is the Rust equivalent of C++ `FallocFileAllocationIterator`.
/// It completes in a single `allocate_chunk()` call by invoking the
/// platform-native preallocation syscall through the `DiskAdaptor`.
///
/// On Linux, this calls `fallocate(2)` which allocates zeroed blocks.
/// On macOS, this calls `fcntl(F_PREALLOCATE)` which may not zero-fill.
/// On Windows, this calls `SetFileValidData` which does not zero-fill.
pub struct FallocFileAllocationIterator<D: DiskAdaptor> {
    adaptor: D,
    offset: u64,
    total_length: u64,
    done: bool,
}

impl<D: DiskAdaptor> FallocFileAllocationIterator<D> {
    /// Create a new falloc iterator.
    ///
    /// # Arguments
    /// * `adaptor` — disk adaptor for allocation operations (must have an open
    ///   file with a raw fd/handle available)
    /// * `offset` — current file size
    /// * `total_length` — target file size
    pub fn new(adaptor: D, offset: u64, total_length: u64) -> Self {
        Self {
            adaptor,
            offset,
            total_length,
            done: false,
        }
    }

    /// Get a mutable reference to the underlying adaptor.
    pub fn adaptor_mut(&mut self) -> &mut D {
        &mut self.adaptor
    }
}

#[async_trait]
impl<D: DiskAdaptor> FileAllocationIterator for FallocFileAllocationIterator<D> {
    async fn allocate_chunk(&mut self) -> Result<()> {
        if self.offset < self.total_length {
            // Use the DiskAdaptor's fallocate path via write-and-allocate.
            // We invoke the full allocate_file pipeline which handles
            // platform-specific fallocate with fallbacks.
            let length = self.total_length;
            let secure = false; // Caller controls security at a higher level
            crate::filesystem::file_allocation::allocate_file(
                &mut self.adaptor,
                std::path::Path::new(""),
                length,
                crate::filesystem::file_allocation::AllocationStrategy::Falloc,
                secure,
            )
            .await?;
            self.offset = self.total_length;
        } else {
            // File is already at least total_length; just ensure size is exact.
            self.adaptor.truncate(self.total_length).await?;
            self.offset = self.total_length;
        }
        self.done = true;
        Ok(())
    }

    fn finished(&self) -> bool {
        self.done
    }

    fn current_length(&self) -> u64 {
        self.offset
    }

    fn total_length(&self) -> u64 {
        self.total_length
    }
}

// =========================================================================
// TruncFileAllocationIterator
// =========================================================================

/// Truncation-based allocation iterator using `ftruncate` / `SetEndOfFile`.
///
/// This is the Rust equivalent of C++ `TruncFileAllocationIterator`.
/// It completes in a single `allocate_chunk()` call by truncating the
/// file to the target length. This creates a sparse file — blocks are
/// not physically allocated until written.
pub struct TruncFileAllocationIterator<D: DiskAdaptor> {
    adaptor: D,
    offset: u64,
    total_length: u64,
    done: bool,
}

impl<D: DiskAdaptor> TruncFileAllocationIterator<D> {
    /// Create a new truncation iterator.
    ///
    /// # Arguments
    /// * `adaptor` — disk adaptor for truncation (must have an open file)
    /// * `offset` — current file size
    /// * `total_length` — target file size
    pub fn new(adaptor: D, offset: u64, total_length: u64) -> Self {
        Self {
            adaptor,
            offset,
            total_length,
            done: false,
        }
    }

    /// Get a mutable reference to the underlying adaptor.
    pub fn adaptor_mut(&mut self) -> &mut D {
        &mut self.adaptor
    }
}

#[async_trait]
impl<D: DiskAdaptor> FileAllocationIterator for TruncFileAllocationIterator<D> {
    async fn allocate_chunk(&mut self) -> Result<()> {
        // C++ TruncFileAllocationIterator calls stream->allocate(0, totalLength_, true)
        // which maps to ftruncate. In Rust we use set_len via truncate().
        self.adaptor.truncate(self.total_length).await?;
        // Flush to ensure metadata is persisted before the caller checks file size.
        self.adaptor.flush().await?;
        self.offset = self.total_length;
        self.done = true;
        Ok(())
    }

    fn finished(&self) -> bool {
        self.done
    }

    fn current_length(&self) -> u64 {
        self.offset
    }

    fn total_length(&self) -> u64 {
        self.total_length
    }
}

// =========================================================================
// AdaptiveFileAllocationIterator
// =========================================================================

/// Adaptive allocation iterator that tries `fallocate` first, then falls back
/// to zero-fill.
///
/// This is the Rust equivalent of C++ `AdaptiveFileAllocationIterator` and is
/// the default iterator for `--file-allocation=prealloc`.
///
/// Strategy:
/// 1. On first `allocate_chunk()`, try a small (4 KiB) `fallocate` probe.
/// 2. If it succeeds, switch to `FallocFileAllocationIterator` for the rest.
/// 3. If it fails (e.g. `EOPNOTSUPP`), switch to
///    `SingleFileAllocationIterator` (zero-fill).
///
/// On platforms without `fallocate` (or when no raw fd is available),
/// the zero-fill path is used directly.
pub struct AdaptiveFileAllocationIterator<D: DiskAdaptor + Default> {
    adaptor: D,
    offset: u64,
    total_length: u64,
    /// Inner iterator selected after the fallocate probe.
    inner: Option<AdaptiveInner<D>>,
}

/// Inner iterator variant after the adaptive probe decides the strategy.
enum AdaptiveInner<D: DiskAdaptor + Default> {
    Falloc(FallocFileAllocationIterator<D>),
    Single(SingleFileAllocationIterator<D>),
}

impl<D: DiskAdaptor + Default> AdaptiveFileAllocationIterator<D> {
    /// Create a new adaptive iterator.
    ///
    /// # Arguments
    /// * `adaptor` — disk adaptor (must have an open file)
    /// * `offset` — current file size
    /// * `total_length` — target file size
    pub fn new(adaptor: D, offset: u64, total_length: u64) -> Self {
        Self {
            adaptor,
            offset,
            total_length,
            inner: None,
        }
    }

    /// Try the fallocate probe and select the inner strategy.
    ///
    /// On Linux, we call `fallocate(2)` on a small 4 KiB region.
    /// If it succeeds, the filesystem supports fallocate and we use
    /// `FallocFileAllocationIterator`. If it returns `EOPNOTSUPP`,
    /// we fall back to `SingleFileAllocationIterator` (zero-fill).
    ///
    /// On non-Linux platforms where fallocate is not available or
    /// the raw fd is missing, we go directly to zero-fill.
    async fn probe_and_select(&mut self) -> Result<()> {
        // Probe: try a small fallocate (4 KiB).
        let probe_len = 4 * 1024u64;
        let remaining = self.total_length.saturating_sub(self.offset);
        let probe_len = probe_len.min(remaining);

        if probe_len == 0 {
            // Nothing to allocate.
            return Ok(());
        }

        // Try the fallocate path. We use the file_allocation::allocate_file
        // function which handles platform-specific fallocate with EOPNOTSUPP
        // fallback.
        let probe_result = crate::filesystem::file_allocation::allocate_file(
            &mut self.adaptor,
            std::path::Path::new(""),
            self.offset + probe_len,
            crate::filesystem::file_allocation::AllocationStrategy::Falloc,
            false,
        )
        .await;

        match probe_result {
            Ok(()) => {
                // fallocate succeeded — filesystem supports it.
                // The probe already allocated up to offset + probe_len.
                self.offset += probe_len;
                if self.offset >= self.total_length {
                    // Small file, fully allocated by the probe.
                    return Ok(());
                }
                // Continue with fallocate for the remaining region.
                self.inner = Some(AdaptiveInner::Falloc(FallocFileAllocationIterator::new(
                    std::mem::take(&mut self.adaptor),
                    self.offset,
                    self.total_length,
                )));
            }
            Err(_) => {
                // fallocate failed — fall back to zero-fill.
                tracing::debug!("Adaptive: fallocate probe failed, falling back to zero-fill");
                let mut single = SingleFileAllocationIterator::new(
                    std::mem::take(&mut self.adaptor),
                    self.offset,
                    self.total_length,
                );
                single.init();
                self.inner = Some(AdaptiveInner::Single(single));
            }
        }

        Ok(())
    }
}

#[async_trait]
impl<D: DiskAdaptor + Default> FileAllocationIterator for AdaptiveFileAllocationIterator<D> {
    async fn allocate_chunk(&mut self) -> Result<()> {
        if self.inner.is_none() {
            self.probe_and_select().await?;
        }

        match &mut self.inner {
            Some(AdaptiveInner::Falloc(falloc)) => falloc.allocate_chunk().await,
            Some(AdaptiveInner::Single(single)) => single.allocate_chunk().await,
            None => {
                // probe_and_select determined nothing to allocate.
                Ok(())
            }
        }
    }

    fn finished(&self) -> bool {
        match &self.inner {
            Some(AdaptiveInner::Falloc(falloc)) => falloc.finished(),
            Some(AdaptiveInner::Single(single)) => single.finished(),
            None => self.offset >= self.total_length,
        }
    }

    fn current_length(&self) -> u64 {
        match &self.inner {
            Some(AdaptiveInner::Falloc(falloc)) => falloc.current_length(),
            Some(AdaptiveInner::Single(single)) => single.current_length(),
            None => self.offset,
        }
    }

    fn total_length(&self) -> u64 {
        self.total_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::disk_adaptor::DirectDiskAdaptor;

    #[tokio::test]
    async fn test_trunc_iterator_completes_in_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_trunc_iter.bin");

        let mut adaptor = DirectDiskAdaptor::new();
        adaptor.open(&path).await.unwrap();

        let mut iter = TruncFileAllocationIterator::new(adaptor, 0, 4096);
        assert!(!iter.finished());
        iter.allocate_chunk().await.unwrap();
        assert!(iter.finished());
        assert_eq!(iter.current_length(), 4096);
        assert_eq!(iter.total_length(), 4096);

        // Verify file size
        let meta = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(meta.len(), 4096);
    }

    #[tokio::test]
    async fn test_single_file_iterator_chunked_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_single_iter.bin");

        let mut adaptor = DirectDiskAdaptor::new();
        adaptor.open(&path).await.unwrap();
        adaptor.truncate(0).await.unwrap();

        // Allocate 512 KiB (2 chunks of 256 KiB)
        let mut iter = SingleFileAllocationIterator::new(adaptor, 0, 512 * 1024);
        iter.init();

        assert!(!iter.finished());
        iter.allocate_chunk().await.unwrap();
        assert!(!iter.finished());
        assert_eq!(iter.current_length(), 256 * 1024);

        iter.allocate_chunk().await.unwrap();
        assert!(iter.finished());
        assert_eq!(iter.current_length(), 512 * 1024);
    }

    #[tokio::test]
    async fn test_single_file_iterator_truncates_overshoot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_overshoot.bin");

        let mut adaptor = DirectDiskAdaptor::new();
        adaptor.open(&path).await.unwrap();
        adaptor.truncate(0).await.unwrap();

        // 1 byte more than a single chunk — second write overshoots
        let target = BUF_SIZE as u64 + 1;
        let mut iter = SingleFileAllocationIterator::new(adaptor, 0, target);
        iter.init();

        iter.allocate_chunk().await.unwrap();
        assert!(!iter.finished());

        iter.allocate_chunk().await.unwrap();
        assert!(iter.finished());
        assert_eq!(iter.current_length(), target);
    }

    #[tokio::test]
    async fn test_adaptive_iterator_fallback_to_zero_fill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_adaptive.bin");

        let mut adaptor = DirectDiskAdaptor::new();
        adaptor.open(&path).await.unwrap();
        adaptor.truncate(0).await.unwrap();

        // On any platform, adaptive should complete allocation.
        let mut iter = AdaptiveFileAllocationIterator::new(adaptor, 0, 1024 * 1024);
        while !iter.finished() {
            iter.allocate_chunk().await.unwrap();
        }

        assert_eq!(iter.current_length(), 1024 * 1024);
    }

    #[tokio::test]
    async fn test_trunc_iterator_with_existing_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_trunc_existing.bin");

        // Pre-create a file with some content
        tokio::fs::write(&path, b"hello").await.unwrap();

        let mut adaptor = DirectDiskAdaptor::new();
        adaptor.open(&path).await.unwrap();

        let mut iter = TruncFileAllocationIterator::new(adaptor, 5, 1024);
        iter.allocate_chunk().await.unwrap();
        assert!(iter.finished());
        assert_eq!(iter.current_length(), 1024);
    }
}
