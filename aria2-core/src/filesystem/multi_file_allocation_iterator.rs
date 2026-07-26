//! Multi-file allocation iterator for BitTorrent torrent downloads.
//!
//! This is the Rust equivalent of C++ `MultiFileAllocationIterator`. It
//! iterates through all files in a `MultiDiskAdaptor`, allocating each file
//! individually using the configured strategy (`Falloc`, `Trunc`, or
//! `Adaptive`).
//!
//! # Architecture
//!
//! ```text
//! MultiFileAllocationIterator
//!   ├── multi_adaptor  — reference to the MultiDiskAdaptor
//!   ├── entry_index    — current file being allocated
//!   ├── inner_iter     — per-file FileAllocationIterator
//!   └── strategy       — allocation method (Falloc/Trunc/Adaptive)
//! ```
//!
//! # C++ Reference
//!
//! - `MultiFileAllocationIterator.h/.cc`
//! - Iterates `DiskWriterEntries` from `MultiDiskAdaptor`
//! - For each entry that needs allocation, creates a per-file iterator
//! - Uses a dedicated `DiskWriter` for each file (not the shared one from
//!   `OpenedFileCounter`) to avoid reopen issues

use async_trait::async_trait;
use tracing::debug;

use crate::error::Result;
use crate::filesystem::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use crate::filesystem::file_allocation::AllocationStrategy;
use crate::filesystem::file_allocation_iterator::FileAllocationIterator;
use crate::filesystem::multi_disk_adaptor::MultiDiskAdaptor;

/// Multi-file allocation iterator that allocates each file in a torrent
/// individually.
///
/// This mirrors the C++ `MultiFileAllocationIterator` which:
/// 1. Iterates through `DiskWriterEntry` list
/// 2. For each entry needing allocation, creates a dedicated `DiskWriter`
/// 3. Selects the per-file iterator based on the allocation method
/// 4. Advances one chunk at a time, yielding between chunks
///
/// # Progress Reporting
///
/// [`current_length`] and [`total_length`] report progress for the
/// *currently active file*, not the overall torrent. This matches the
/// C++ behavior where the download engine aggregates progress at a
/// higher level.
pub struct MultiFileAllocationIterator {
    /// The multi-disk adaptor whose entries we iterate.
    multi_adaptor: MultiDiskAdaptor,
    /// Current entry index being processed.
    entry_index: usize,
    /// Per-file allocation iterator for the current entry.
    inner_iter: Option<PerFileIter>,
    /// Allocation strategy to use for each file.
    strategy: AllocationStrategy,
    /// Whether to zero-fill after fallocate on platforms that don't zero-fill.
    #[allow(dead_code)]
    secure_falloc: bool,
}

/// Per-file iterator that holds a dedicated `DirectDiskAdaptor` and a
/// `FileAllocationIterator`. This replaces the C++ pattern of creating
/// a dedicated `DiskWriter` for each file to avoid reopen issues with
/// `OpenedFileCounter`.
enum PerFileIter {
    Falloc(
        crate::filesystem::file_allocation_iterator::FallocFileAllocationIterator<
            DirectDiskAdaptor,
        >,
    ),
    Trunc(
        crate::filesystem::file_allocation_iterator::TruncFileAllocationIterator<DirectDiskAdaptor>,
    ),
    Single(
        crate::filesystem::file_allocation_iterator::SingleFileAllocationIterator<
            DirectDiskAdaptor,
        >,
    ),
}

impl MultiFileAllocationIterator {
    /// Create a new multi-file allocation iterator.
    ///
    /// # Arguments
    /// * `multi_adaptor` — the `MultiDiskAdaptor` containing file entries
    /// * `strategy` — allocation strategy (`Falloc`, `Trunc`, or `Adaptive`)
    /// * `secure_falloc` — zero-fill after fallocate on non-zeroing platforms
    pub fn new(
        multi_adaptor: MultiDiskAdaptor,
        strategy: AllocationStrategy,
        secure_falloc: bool,
    ) -> Self {
        Self {
            multi_adaptor,
            entry_index: 0,
            inner_iter: None,
            strategy,
            secure_falloc,
        }
    }

    /// Advance to the next file needing allocation and create its iterator.
    async fn advance_to_next_file(&mut self) -> Result<bool> {
        // Close any existing inner iterator's file handle.
        self.inner_iter = None;

        let entries = self.multi_adaptor.get_disk_writer_entries();
        while self.entry_index < entries.len() {
            let entry = &entries[self.entry_index];

            // Skip entries without a disk writer (no file to allocate).
            if !entry.has_disk_writer() {
                self.entry_index += 1;
                continue;
            }

            let target_length = entry.get_file_entry().get_length();

            // Get current file size on disk (0 if file doesn't exist yet).
            let current_size = match tokio::fs::metadata(entry.get_file_path()).await {
                Ok(meta) => meta.len(),
                Err(_) => 0,
            };

            if !entry.needs_file_allocation() || current_size >= target_length {
                debug!(
                    "Skipping allocation for {:?}: current={}, target={}",
                    entry.get_file_path(),
                    current_size,
                    target_length
                );
                self.entry_index += 1;
                continue;
            }

            debug!(
                "Allocating file {:?}: target size={}, current size={}",
                entry.get_file_path(),
                target_length,
                current_size
            );

            // Create parent directories if needed.
            if let Some(parent) = entry.get_file_path().parent() {
                if !parent.exists() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        crate::error::Aria2Error::Io(format!("create_dir_all {:?}: {}", parent, e))
                    })?;
                }
            }

            // Open a dedicated DirectDiskAdaptor for this file (mirrors C++
            // creating a dedicated DiskWriter per file).
            // DirectDiskAdaptor::open creates the file if it doesn't exist.
            let mut adaptor = DirectDiskAdaptor::new();
            adaptor.open(entry.get_file_path()).await?;

            self.inner_iter = Some(self.create_per_file_iter(adaptor, current_size, target_length));
            return Ok(true);
        }

        // No more entries to allocate.
        Ok(false)
    }

    /// Create the appropriate per-file iterator based on the allocation
    /// strategy.
    fn create_per_file_iter(
        &self,
        adaptor: DirectDiskAdaptor,
        offset: u64,
        total_length: u64,
    ) -> PerFileIter {
        match self.strategy {
            AllocationStrategy::Falloc | AllocationStrategy::Mmap => PerFileIter::Falloc(
                crate::filesystem::file_allocation_iterator::FallocFileAllocationIterator::new(
                    adaptor,
                    offset,
                    total_length,
                ),
            ),
            AllocationStrategy::Trunc => PerFileIter::Trunc(
                crate::filesystem::file_allocation_iterator::TruncFileAllocationIterator::new(
                    adaptor,
                    offset,
                    total_length,
                ),
            ),
            // Prealloc and None both use zero-fill (Adaptive would try falloc
            // first; we use Single here for simplicity — the fallocate probe
            // is already handled at a higher level by AdaptiveFileAllocationIterator
            // for single-file downloads. For multi-file, C++ also uses
            // AdaptiveFileAllocationIterator as default, which falls back to
            // SingleFileAllocationIterator).
            AllocationStrategy::Prealloc | AllocationStrategy::None => {
                let mut iter =
                    crate::filesystem::file_allocation_iterator::SingleFileAllocationIterator::new(
                        adaptor,
                        offset,
                        total_length,
                    );
                iter.init();
                PerFileIter::Single(iter)
            }
        }
    }

    /// Get the allocation strategy.
    pub fn strategy(&self) -> AllocationStrategy {
        self.strategy
    }

    /// Get a reference to the underlying `MultiDiskAdaptor`.
    pub fn multi_adaptor(&self) -> &MultiDiskAdaptor {
        &self.multi_adaptor
    }
}

#[async_trait]
impl FileAllocationIterator for MultiFileAllocationIterator {
    async fn allocate_chunk(&mut self) -> Result<()> {
        // If we have an active inner iterator, use it.
        if let Some(inner) = &mut self.inner_iter {
            let done = match inner {
                PerFileIter::Falloc(f) => {
                    f.allocate_chunk().await?;
                    f.finished()
                }
                PerFileIter::Trunc(t) => {
                    t.allocate_chunk().await?;
                    t.finished()
                }
                PerFileIter::Single(s) => {
                    s.allocate_chunk().await?;
                    s.finished()
                }
            };

            if done {
                self.entry_index += 1;
                // Explicitly close the per-file adaptor before advancing.
                // C++ MultiFileAllocationIterator calls diskWriter_->closeFile().
                // On Windows, tokio::fs::File::drop closes asynchronously,
                // so we must close explicitly to ensure data is persisted.
                if let Some(inner) = self.inner_iter.take() {
                    match inner {
                        PerFileIter::Falloc(mut f) => {
                            let _ = f.adaptor_mut().close().await;
                        }
                        PerFileIter::Trunc(mut t) => {
                            let _ = t.adaptor_mut().close().await;
                        }
                        PerFileIter::Single(mut s) => {
                            let _ = s.adaptor_mut().close().await;
                        }
                    }
                    // Yield to allow the async file close to complete before
                    // any subsequent metadata checks (important on Windows).
                    tokio::task::yield_now().await;
                }
                // Try to advance to the next file.
                self.advance_to_next_file().await?;
            }
        } else {
            // No inner iterator — try to find the next file.
            let found = self.advance_to_next_file().await?;
            if found {
                // Recursively call to allocate the first chunk.
                return self.allocate_chunk().await;
            }
        }

        Ok(())
    }

    fn finished(&self) -> bool {
        // Check if we've advanced past all entries AND have no active inner iter.
        // Note: if entry_index == 0 and inner_iter is None, we haven't started
        // advancing yet. In that case, we're not finished — allocate_chunk()
        // must be called to trigger advance_to_next_file().
        if self.entry_index == 0 && self.inner_iter.is_none() {
            // Peek: check if ANY entry actually needs allocation.
            // If all entries are already at target size or don't need allocation,
            // then we're effectively finished without needing to allocate.
            let entries = self.multi_adaptor.get_disk_writer_entries();
            if entries.is_empty() {
                return true;
            }
            // Quick check: if the first entry needs allocation, we're not done.
            // We can't easily check all entries here without async, so return false
            // to force allocate_chunk() to be called.
            return false;
        }
        let entries = self.multi_adaptor.get_disk_writer_entries();
        let all_done = self.entry_index >= entries.len();
        let inner_done = self.inner_iter.as_ref().map_or(true, |i| match i {
            PerFileIter::Falloc(f) => f.finished(),
            PerFileIter::Trunc(t) => t.finished(),
            PerFileIter::Single(s) => s.finished(),
        });
        all_done && inner_done
    }

    fn current_length(&self) -> u64 {
        match &self.inner_iter {
            Some(PerFileIter::Falloc(f)) => f.current_length(),
            Some(PerFileIter::Trunc(t)) => t.current_length(),
            Some(PerFileIter::Single(s)) => s.current_length(),
            None => 0,
        }
    }

    fn total_length(&self) -> u64 {
        match &self.inner_iter {
            Some(PerFileIter::Falloc(f)) => f.total_length(),
            Some(PerFileIter::Trunc(t)) => t.total_length(),
            Some(PerFileIter::Single(s)) => s.total_length(),
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::disk_adaptor::DirectDiskAdaptor;
    use crate::filesystem::file_allocation_iterator::FileAllocationIterator;
    use crate::filesystem::multi_disk_adaptor::FileEntry;

    /// Direct test: create a file, open with DirectDiskAdaptor, truncate
    /// to a larger size, verify the file size.
    #[tokio::test]
    async fn test_direct_disk_adaptor_truncate_extend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_extend.bin");

        // Create file with 512 bytes
        tokio::fs::write(&path, vec![0u8; 512]).await.unwrap();

        // Open and truncate to 1024
        let mut adaptor = DirectDiskAdaptor::new();
        adaptor.open(&path).await.unwrap();
        adaptor.truncate(1024).await.unwrap();
        adaptor.flush().await.unwrap();
        adaptor.close().await.unwrap();

        // Verify size
        let meta = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(meta.len(), 1024, "file should be extended to 1024 bytes");
    }

    /// Test that MultiFileAllocationIterator completes allocation for a
    /// single pre-existing file using Trunc strategy.
    #[tokio::test]
    async fn test_multi_file_single_entry_trunc() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file1.bin");

        // Pre-create file at partial size
        tokio::fs::write(&file_path, vec![0u8; 512]).await.unwrap();

        // FileEntry::new(path, length, offset, is_requested)
        // We want: length=1024, offset=0
        let entries = vec![FileEntry::new(file_path.clone(), 1024, 0, true)];

        let mut adaptor = MultiDiskAdaptor::new(1024);
        adaptor.set_file_entries(entries);

        let mut iter = MultiFileAllocationIterator::new(adaptor, AllocationStrategy::Trunc, false);

        while !iter.finished() {
            iter.allocate_chunk().await.unwrap();
        }

        // Verify file was extended to the target length.
        let meta = tokio::fs::metadata(&file_path).await.unwrap();
        assert_eq!(meta.len(), 1024, "file should be extended to 1024 bytes");
    }

    /// Test that MultiFileAllocationIterator skips files that are already
    /// at the target size.
    #[tokio::test]
    async fn test_multi_file_skips_allocated() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file1.bin");

        // Pre-create file at target size
        tokio::fs::write(&file_path, vec![0u8; 1024]).await.unwrap();

        // FileEntry::new(path, length, offset, is_requested)
        // We want: length=1024, offset=0
        let entries = vec![FileEntry::new(file_path.clone(), 1024, 0, true)];

        let mut adaptor = MultiDiskAdaptor::new(1024);
        adaptor.set_file_entries(entries);

        let mut iter = MultiFileAllocationIterator::new(adaptor, AllocationStrategy::Trunc, false);

        // Call allocate_chunk() which will advance through entries and
        // skip already-allocated files.
        while !iter.finished() {
            iter.allocate_chunk().await.unwrap();
        }

        // File should still be at original size (not changed)
        let meta = tokio::fs::metadata(&file_path).await.unwrap();
        assert_eq!(meta.len(), 1024);
    }
}
