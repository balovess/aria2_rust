//! Multi-file disk adaptor that maps a contiguous torrent byte stream to
//! individual files on disk.
//!
//! This is the Rust equivalent of the C++ aria2 `MultiDiskAdaptor` class.
//! It handles cross-file writes/reads, shared-piece analysis, lazy file
//! opening, and max-open-files eviction.
//!
//! # Architecture
//!
//! ```text
//! MultiDiskAdaptor
//!   ├── piece_length           — piece boundary for shared-piece analysis
//!   ├── disk_writer_entries    — all entries, sorted by FileEntry offset
//!   ├── opened_entries         — indices of currently-open entries (LRU cache)
//!   ├── read_only              — whether files are opened read-only
//!   └── max_open_files         — file descriptor limit (default 100)
//! ```
//!
//! # Cross-file I/O
//!
//! A single write/read at a global offset may span multiple files. The
//! algorithm uses binary search (`find_first_entry_index`) to locate the
//! first entry containing the offset, then iterates across entries until
//! all data is written/read.

use std::any::Any;
use std::path::Path;

use async_trait::async_trait;
use rand::Rng;
use tracing::{debug, trace, warn};

use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_adaptor::DiskAdaptor;

use super::disk_writer_entry::{DiskWriterEntry, OpenMode};
use super::file_entry::FileEntry;

/// Default maximum number of simultaneously open file descriptors.
const DEFAULT_MAX_OPEN_FILES: usize = 100;

/// Multi-file disk adaptor that maps a contiguous torrent byte stream to
/// individual files on disk.
///
/// Handles:
/// - Cross-file writes and reads (data spanning multiple files)
/// - Shared-piece analysis (forward + backward scan)
/// - Lazy file opening (open-on-first-access)
/// - Max-open-files eviction (random replacement)
///
/// This is the Rust equivalent of the C++ `MultiDiskAdaptor`.
pub struct MultiDiskAdaptor {
    /// Piece length for shared-piece boundary calculation.
    piece_length: u32,
    /// All entries, sorted by `FileEntry::offset`. The entry at index 0
    /// has the smallest offset.
    disk_writer_entries: Vec<DiskWriterEntry>,
    /// Indices into `disk_writer_entries` for currently-open entries.
    /// Used as an LRU-like cache for the open file limit.
    pub(crate) opened_entries: Vec<usize>,
    /// Whether files are opened in read-only mode.
    read_only: bool,
    /// Maximum number of simultaneously open file descriptors.
    max_open_files: usize,
}

impl MultiDiskAdaptor {
    /// Create a new `MultiDiskAdaptor` with the given piece length.
    ///
    /// File entries must be set via [`set_file_entries`] before calling
    /// any open method.
    pub fn new(piece_length: u32) -> Self {
        Self {
            piece_length,
            disk_writer_entries: Vec::new(),
            opened_entries: Vec::new(),
            read_only: false,
            max_open_files: DEFAULT_MAX_OPEN_FILES,
        }
    }

    /// Set the file entries. Must be called before opening files.
    ///
    /// The entries are sorted by offset. Previous entries and opened files
    /// are closed and discarded.
    pub fn set_file_entries(&mut self, entries: Vec<FileEntry>) {
        // Entries should already be sorted by offset (as in the torrent
        // info dict), but sort defensively.
        let mut entries = entries;
        entries.sort_by_key(|e| e.offset);
        self.disk_writer_entries = entries.into_iter().map(DiskWriterEntry::new).collect();
    }

    /// Set the maximum number of simultaneously open files.
    pub fn set_max_open_files(&mut self, max: usize) {
        self.max_open_files = max.max(1);
    }

    /// Set the piece length.
    pub fn set_piece_length(&mut self, piece_length: u32) {
        self.piece_length = piece_length;
    }

    /// Get the piece length.
    pub fn get_piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Enable read-only mode. Files will be opened without write access.
    pub fn enable_read_only(&mut self) {
        self.read_only = true;
    }

    /// Disable read-only mode.
    pub fn disable_read_only(&mut self) {
        self.read_only = false;
    }

    /// Whether read-only mode is enabled.
    pub fn is_read_only_enabled(&self) -> bool {
        self.read_only
    }

    // ── Open strategies ──────────────────────────────────────────────

    /// Open all files with truncation (fresh download).
    ///
    /// Calls [`reset_disk_writer_entries`] then opens each entry.
    pub async fn init_and_open_file(&mut self) -> Result<()> {
        self.reset_disk_writer_entries();
        for idx in 0..self.disk_writer_entries.len() {
            self.open_if_not(idx, OpenMode::InitAndOpen).await?;
        }
        Ok(())
    }

    /// Open all files without truncation.
    ///
    /// Ensures zero-length files are created on disk.
    pub async fn open_file(&mut self) -> Result<()> {
        self.reset_disk_writer_entries();
        for idx in 0..self.disk_writer_entries.len() {
            self.open_if_not(idx, OpenMode::Open).await?;
        }
        Ok(())
    }

    /// Create entries but do NOT open any files (lazy open on read/write).
    ///
    /// This is used for resume scenarios where files already exist on disk.
    pub async fn open_existing_file(&mut self) -> Result<()> {
        self.reset_disk_writer_entries();
        // No files are opened — they will be opened lazily on first access.
        Ok(())
    }

    /// Close all opened files and clear the opened-entries cache.
    pub async fn close_file(&mut self) {
        for &idx in &self.opened_entries {
            self.disk_writer_entries[idx].close_file().await;
        }
        self.opened_entries.clear();
    }

    // ── Cross-file I/O ───────────────────────────────────────────────

    /// Write `data` at global `offset` in the torrent byte stream.
    ///
    /// The write may span multiple files. Files are lazily opened with
    /// [`OpenMode::Open`] (no truncation).
    pub async fn write_data(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let first_idx = self.find_first_entry_index(offset)?;
        let first_entry = &self.disk_writer_entries[first_idx];
        let mut file_offset = offset - first_entry.file_entry.get_offset();
        let mut rem = data.len();
        let data_len = data.len();

        for idx in first_idx..self.disk_writer_entries.len() {
            let write_length = self.calculate_write_length(idx, file_offset, rem);
            self.open_if_not(idx, OpenMode::Open).await?;

            if !self.disk_writer_entries[idx].is_open() {
                return Err(Aria2Error::Io(format!(
                    "DiskWriter for offset={}, filename={:?} is not opened",
                    offset + (data_len - rem) as u64,
                    self.disk_writer_entries[idx].get_file_path()
                )));
            }

            let write_start = data_len - rem;
            self.disk_writer_entries[idx]
                .write_at(file_offset, &data[write_start..write_start + write_length])
                .await?;

            rem -= write_length;
            file_offset = 0; // Subsequent files start at offset 0.
            if rem == 0 {
                break;
            }
        }

        Ok(())
    }

    /// Read `length` bytes from global `offset` in the torrent byte stream.
    ///
    /// The read may span multiple files. Files are lazily opened with
    /// [`OpenMode::Open`]. Returns the data as a `Vec<u8>`.
    pub async fn read_data(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        self.read_data_internal(offset, length, false).await
    }

    /// Read `length` bytes and drop OS page cache after each file read.
    pub async fn read_data_drop_cache(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        self.read_data_internal(offset, length, true).await
    }

    /// Internal read with optional cache drop.
    async fn read_data_internal(
        &mut self,
        offset: u64,
        length: u64,
        _drop_cache: bool,
    ) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }

        let first_idx = self.find_first_entry_index(offset)?;
        let first_entry = &self.disk_writer_entries[first_idx];
        let mut file_offset = offset - first_entry.file_entry.get_offset();
        let mut rem = length as usize;
        let mut result = vec![0u8; length as usize];
        let mut total_read = 0usize;

        for idx in first_idx..self.disk_writer_entries.len() {
            let read_length = self.calculate_write_length(idx, file_offset, rem);
            self.open_if_not(idx, OpenMode::Open).await?;

            if !self.disk_writer_entries[idx].is_open() {
                return Err(Aria2Error::Io(format!(
                    "DiskWriter for offset={}, filename={:?} is not opened",
                    offset + total_read as u64,
                    self.disk_writer_entries[idx].get_file_path()
                )));
            }

            // Inner loop handles short reads (partial reads at EOF).
            let mut inner_rem = read_length;
            while inner_rem > 0 {
                let buf_start = total_read;
                let buf_end = buf_start + inner_rem;
                let nread = self.disk_writer_entries[idx]
                    .read_at(file_offset, &mut result[buf_start..buf_end])
                    .await?;

                if nread == 0 {
                    // EOF reached — return what we have.
                    result.truncate(total_read);
                    return Ok(result);
                }

                total_read += nread;
                inner_rem -= nread;
                file_offset += nread as u64;

                // Cache drop is a no-op on most platforms; reserved for
                // POSIX fadvise(DONTNEED) integration.
                #[cfg(unix)]
                if _drop_cache {
                    // TODO: integrate posix_fadvise(DONTNEED) for cache drop.
                }
            }

            file_offset = 0; // Subsequent files start at offset 0.
            rem -= read_length;
            if rem == 0 {
                break;
            }
        }

        result.truncate(total_read);
        Ok(result)
    }

    // ── Auxiliary methods ────────────────────────────────────────────

    /// Flush OS buffers for all opened disk writers.
    pub async fn flush_os_buffers(&mut self) -> Result<()> {
        for &idx in &self.opened_entries {
            self.disk_writer_entries[idx].flush().await?;
        }
        Ok(())
    }

    /// Whether any file entry exists on disk.
    pub fn file_exists(&self) -> bool {
        self.disk_writer_entries
            .iter()
            .any(|e| e.file_entry.exists())
    }

    /// Sum of actual file sizes on disk for all entries.
    pub async fn size(&self) -> Result<u64> {
        let mut total: u64 = 0;
        for entry in &self.disk_writer_entries {
            if entry.file_entry.exists() {
                match tokio::fs::metadata(entry.file_entry.get_path()).await {
                    Ok(meta) => total += meta.len(),
                    Err(e) => {
                        warn!(
                            "Failed to get metadata for {:?}: {}",
                            entry.file_entry.get_path(),
                            e
                        );
                    }
                }
            }
        }
        Ok(total)
    }

    /// Truncate files that are larger than their declared length.
    ///
    /// For each entry where the on-disk size exceeds the declared length,
    /// the file is opened (lazily) and truncated.
    pub async fn cut_trailing_garbage(&mut self) -> Result<()> {
        for idx in 0..self.disk_writer_entries.len() {
            let declared_length = self.disk_writer_entries[idx].file_entry.get_length();
            // Check on-disk size without opening the entry's file handle.
            let on_disk_size = match tokio::fs::metadata(
                self.disk_writer_entries[idx].file_entry.get_path(),
            )
            .await
            {
                Ok(meta) => meta.len(),
                Err(_) => continue, // File doesn't exist — skip.
            };

            if on_disk_size > declared_length {
                self.open_if_not(idx, OpenMode::Open).await?;
                self.disk_writer_entries[idx]
                    .truncate(declared_length)
                    .await?;
                debug!(
                    "cutTrailingGarbage: truncated {:?} from {} to {}",
                    self.disk_writer_entries[idx].file_entry.get_path(),
                    on_disk_size,
                    declared_length
                );
            }
        }
        Ok(())
    }

    /// Randomly close `num_close` opened files to free file descriptors.
    ///
    /// Returns the number of files actually closed (may be less than
    /// `num_close` if fewer files are open).
    pub async fn try_close_file(&mut self, num_close: usize) -> usize {
        let mut left = num_close;
        while !self.opened_entries.is_empty() && left > 0 {
            let index = rand::thread_rng().gen_range(0..self.opened_entries.len());
            let entry_idx = self.opened_entries[index];
            self.disk_writer_entries[entry_idx].close_file().await;
            // Swap-remove to maintain valid indices.
            self.opened_entries.swap_remove(index);
            left -= 1;
        }
        num_close - left
    }

    /// Returns a reference to the disk writer entries.
    pub fn get_disk_writer_entries(&self) -> &[DiskWriterEntry] {
        &self.disk_writer_entries
    }

    // ── Private: shared-piece analysis ────────────────────────────────

    /// Build the full entry list with shared-piece analysis.
    ///
    /// This corresponds to `MultiDiskAdaptor::resetDiskWriterEntries()` in C++.
    /// Performs:
    /// 1. Forward scan — determines `needs_disk_writer` for non-requested
    ///    files that share a piece with a requested file.
    /// 2. Backward scan — determines `needs_file_allocation` for non-requested
    ///    files that need pre-sizing on disk.
    /// 3. Disk writer creation — entries with `needs_file_allocation`,
    ///    `needs_disk_writer`, or existing on-disk files are eligible for I/O.
    fn reset_disk_writer_entries(&mut self) {
        assert!(
            self.opened_entries.is_empty(),
            "resetDiskWriterEntries called with open files"
        );

        if self.disk_writer_entries.is_empty() {
            return;
        }

        // Reset flags: each entry starts with needs_file_allocation = is_requested,
        // needs_disk_writer = false (as set in DiskWriterEntry::new).
        // Since we may be called multiple times, re-derive the flags.
        for entry in &mut self.disk_writer_entries {
            entry.needs_file_allocation = entry.file_entry.is_requested();
            entry.needs_disk_writer = false;
        }

        if self.piece_length == 0 {
            // piece_length == 0 is used for unit testing only (C++ comment).
            // Skip shared-piece analysis.
            return;
        }

        let pl = self.piece_length as u64;

        // ── Forward scan: determine needs_disk_writer ──
        //
        // For each entry in offset order:
        //   If requested and length > 0:
        //     lastOffset = ceil(lastOffset / pieceLength) * pieceLength
        //     where lastOffset is the end of the last requested file's piece
        //   If not requested and file offset < lastOffset:
        //     needs_disk_writer = true
        let mut last_offset: u64 = 0;
        for entry in &mut self.disk_writer_entries {
            if entry.file_entry.is_requested() {
                if entry.file_entry.get_length() > 0 {
                    // C++: (lastOffset - 1) / pieceLength * pieceLength + pieceLength
                    // This is: ceil(lastOffset / pieceLength) * pieceLength
                    // where lastOffset = fileEntry->getLastOffset()
                    let file_last_offset = entry.file_entry.get_last_offset();
                    last_offset = ((file_last_offset - 1) / pl) * pl + pl;
                }
            } else if entry.file_entry.get_offset() < last_offset {
                debug!(
                    "{} needs DiskWriter (forward scan)",
                    entry.file_entry.get_path().display()
                );
                entry.needs_disk_writer = true;
            }
        }

        // ── Backward scan: determine needs_file_allocation ──
        //
        // For each entry in REVERSE offset order:
        //   If requested:
        //     lastOffset = floor(offset / pieceLength) * pieceLength
        //   If not requested and (lastOffset <= offset OR lastOffset < lastOffset_of_file):
        //     needs_file_allocation = true
        let mut last_offset: u64 = u64::MAX;
        for entry in self.disk_writer_entries.iter_mut().rev() {
            if entry.file_entry.is_requested() {
                last_offset = (entry.file_entry.get_offset() / pl) * pl;
            } else if last_offset <= entry.file_entry.get_offset()
                || last_offset < entry.file_entry.get_last_offset()
            {
                debug!(
                    "{} needs file allocation (backward scan)",
                    entry.file_entry.get_path().display()
                );
                entry.needs_file_allocation = true;
            }
        }

        // Note: In C++, this is where DefaultDiskWriterFactory creates
        // DiskWriter objects for entries that need them. In Rust, we don't
        // pre-create file handles — files are opened lazily via open_if_not.
        // The eligibility check (needs_file_allocation || needs_disk_writer ||
        // fileExists) is performed at open time.
    }

    // ── Private: lazy open ───────────────────────────────────────────

    /// Open the file at `idx` if not already open, tracking it in the
    /// opened-entries cache.
    ///
    /// If the max-open-files limit would be exceeded, evict a randomly
    /// chosen entry first.
    async fn open_if_not(&mut self, idx: usize, mode: OpenMode) -> Result<()> {
        if self.disk_writer_entries[idx].is_open() {
            // Cache hit — no action needed.
            return Ok(());
        }

        // Cache miss — ensure we're under the file descriptor limit.
        if self.opened_entries.len() >= self.max_open_files {
            // Evict one entry randomly.
            let evict_idx = rand::thread_rng().gen_range(0..self.opened_entries.len());
            let evict_entry = self.opened_entries[evict_idx];
            self.disk_writer_entries[evict_entry].close_file().await;
            self.opened_entries.swap_remove(evict_idx);
            trace!(
                "Evicted file handle for {:?}",
                self.disk_writer_entries[evict_entry].get_file_path()
            );
        }

        // Only open if the entry is eligible for I/O.
        // This mirrors the C++ check: needsFileAllocation || needsDiskWriter || fileExists.
        if !self.disk_writer_entries[idx].has_disk_writer() {
            return Ok(());
        }

        self.disk_writer_entries[idx]
            .open_with_mode(mode, self.read_only)
            .await?;
        self.opened_entries.push(idx);
        Ok(())
    }

    // ── Private: binary search ───────────────────────────────────────

    /// Find the index of the first `DiskWriterEntry` whose byte range
    /// contains `offset`.
    ///
    /// Uses binary search (upper_bound / partition_point), matching the C++
    /// `findFirstDiskWriterEntry` algorithm.
    ///
    /// # Errors
    ///
    /// Returns `Aria2Error::Io` if `offset` is out of range of all entries.
    pub(crate) fn find_first_entry_index(&self, offset: u64) -> Result<usize> {
        if self.disk_writer_entries.is_empty() {
            return Err(Aria2Error::Io(format!(
                "File offset {} out of range (no entries)",
                offset
            )));
        }

        // Find the first entry whose offset > search_offset, then step back.
        let partition = self
            .disk_writer_entries
            .partition_point(|e| e.file_entry.get_offset() <= offset);

        if partition == 0 {
            // offset is before the first entry's offset.
            return Err(Aria2Error::Io(format!(
                "File offset {} out of range (before first file at offset {})",
                offset,
                self.disk_writer_entries[0].file_entry.get_offset()
            )));
        }

        let idx = partition - 1;
        let entry = &self.disk_writer_entries[idx];

        // Validate: entry.offset <= offset && offset < entry.lastOffset
        if offset >= entry.file_entry.get_last_offset() {
            return Err(Aria2Error::Io(format!(
                "File offset {} out of range (past end of file {:?} at offset {})",
                offset,
                entry.file_entry.get_path(),
                entry.file_entry.get_last_offset()
            )));
        }

        Ok(idx)
    }

    /// Calculate how many bytes can be written/read in the entry at `idx`
    /// starting at `file_offset`, given `rem` bytes remaining.
    ///
    /// Corresponds to the C++ `calculateLength` helper.
    fn calculate_write_length(&self, idx: usize, file_offset: u64, rem: usize) -> usize {
        let file_length = self.disk_writer_entries[idx].file_entry.get_length();
        let rem_u64 = rem as u64;

        if file_length < file_offset + rem_u64 {
            (file_length - file_offset) as usize
        } else {
            rem
        }
    }
}

// =========================================================================
// DiskAdaptor trait implementation
// =========================================================================

#[async_trait]
impl DiskAdaptor for MultiDiskAdaptor {
    /// Open all files without truncation.
    ///
    /// The `path` argument is ignored — file paths come from the
    /// [`FileEntry`] list set via [`set_file_entries`].
    async fn open(&mut self, _path: &Path) -> Result<()> {
        self.open_file().await
    }

    /// Write `data` at global `offset` in the torrent byte stream.
    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.write_data(offset, data).await
    }

    /// Read `length` bytes from global `offset` in the torrent byte stream.
    async fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        self.read_data(offset, length).await
    }

    /// Close all opened files.
    async fn close(&mut self) -> Result<()> {
        self.close_file().await;
        Ok(())
    }

    /// Truncate is not directly meaningful for multi-file adaptors.
    ///
    /// Returns an error since there is no single file to truncate.
    async fn truncate(&mut self, _length: u64) -> Result<()> {
        Err(Aria2Error::Io(
            "truncate not supported on MultiDiskAdaptor".to_string(),
        ))
    }

    /// Flush OS buffers for all opened files.
    async fn flush(&mut self) -> Result<()> {
        self.flush_os_buffers().await
    }

    /// Sum of actual file sizes on disk.
    async fn size(&self) -> Result<u64> {
        self.size().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    #[cfg(unix)]
    fn unix_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        // MultiDiskAdaptor manages multiple file descriptors;
        // no single fd is representative.
        None
    }

    #[cfg(windows)]
    fn windows_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        // MultiDiskAdaptor manages multiple file handles;
        // no single handle is representative.
        None
    }
}
