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
//!
//! DiskWriterEntry
//!   ├── file_entry             — path, length, offset, is_requested
//!   ├── file                   — Option<tokio::fs::File> (lazy-opened)
//!   ├── is_open                — whether file handle is currently open
//!   ├── needs_file_allocation  — non-requested file needs pre-sizing
//!   └── needs_disk_writer      — non-requested file shares a piece
//! ```
//!
//! # Cross-file I/O
//!
//! A single write/read at a global offset may span multiple files. The
//! algorithm uses binary search (`find_first_entry_index`) to locate the
//! first entry containing the offset, then iterates across entries until
//! all data is written/read.

use std::any::Any;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rand::Rng;
use tracing::{debug, trace, warn};

use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_adaptor::DiskAdaptor;

// =========================================================================
// FileEntry
// =========================================================================

/// Represents a single file within a multi-file torrent/download.
///
/// Each `FileEntry` describes a contiguous region of the global byte stream:
/// `[offset, offset + length)`. Files are sorted by offset and do not overlap.
///
/// This is the Rust equivalent of the C++ `FileEntry` used by
/// `MultiDiskAdaptor`.
#[derive(Debug, Clone)]
pub struct FileEntry {
    path: PathBuf,
    length: u64,
    offset: u64,
    is_requested: bool,
}

impl FileEntry {
    /// Create a new `FileEntry`.
    ///
    /// # Arguments
    /// * `path` - Absolute path of the file on disk
    /// * `length` - Length of the file in bytes
    /// * `offset` - Global byte offset in the torrent stream
    /// * `is_requested` - Whether this file is part of the download request
    pub fn new(path: PathBuf, length: u64, offset: u64, is_requested: bool) -> Self {
        Self {
            path,
            length,
            offset,
            is_requested,
        }
    }

    /// Returns the file path.
    pub fn get_path(&self) -> &Path {
        &self.path
    }

    /// Returns the global byte offset of this file in the torrent stream.
    pub fn get_offset(&self) -> u64 {
        self.offset
    }

    /// Returns the length of this file in bytes.
    pub fn get_length(&self) -> u64 {
        self.length
    }

    /// Returns the exclusive end offset: `offset + length`.
    pub fn get_last_offset(&self) -> u64 {
        self.offset + self.length
    }

    /// Returns whether this file is part of the download request.
    pub fn is_requested(&self) -> bool {
        self.is_requested
    }

    /// Set the requested flag.
    pub fn set_requested(&mut self, requested: bool) {
        self.is_requested = requested;
    }

    /// Checks whether the file exists on disk.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }
}

// =========================================================================
// OpenMode
// =========================================================================

/// Strategy for opening a file within a `DiskWriterEntry`.
///
/// Replaces the C++ member-function-pointer pattern used in
/// `MultiDiskAdaptor::openIfNot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum OpenMode {
    /// Truncate and open — for fresh downloads.
    InitAndOpen,
    /// Open without truncation — ensures zero-length files exist.
    Open,
    /// Open only if the file already exists — no creation.
    OpenExisting,
}

// =========================================================================
// DiskWriterEntry
// =========================================================================

/// Associates a [`FileEntry`] with an optional file handle and flags
/// controlling allocation and shared-piece behavior.
///
/// This is the Rust equivalent of the C++ `DiskWriterEntry`.
pub struct DiskWriterEntry {
    file_entry: FileEntry,
    file: Option<tokio::fs::File>,
    is_open: bool,
    needs_file_allocation: bool,
    needs_disk_writer: bool,
}

impl DiskWriterEntry {
    /// Create a new entry from a [`FileEntry`].
    ///
    /// The file is not opened; `is_open` starts as `false`.
    /// `needs_file_allocation` is initialized to `file_entry.is_requested()`,
    /// matching the C++ `createDiskWriterEntry` helper.
    fn new(file_entry: FileEntry) -> Self {
        let needs_file_allocation = file_entry.is_requested();
        Self {
            file_entry,
            file: None,
            is_open: false,
            needs_file_allocation,
            needs_disk_writer: false,
        }
    }

    /// Returns the file path.
    pub fn get_file_path(&self) -> &Path {
        self.file_entry.get_path()
    }

    /// Returns a reference to the underlying [`FileEntry`].
    pub fn get_file_entry(&self) -> &FileEntry {
        &self.file_entry
    }

    /// Open the file with truncation (fresh download).
    ///
    /// Creates parent directories if needed, truncates the file to
    /// `file_entry.length`, and marks the entry as open.
    async fn init_and_open_file(&mut self, read_only: bool) -> Result<()> {
        self.ensure_parent_dirs()?;
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(!read_only).read(true).create(true).truncate(true);
        let f = opts
            .open(&self.file_entry.path)
            .await
            .map_err(|e| Aria2Error::Io(format!("initAndOpenFile {:?}: {}", self.file_entry.path, e)))?;
        self.file = Some(f);
        self.is_open = true;
        debug!("initAndOpenFile: {:?}", self.file_entry.path);
        Ok(())
    }

    /// Open the file without truncation.
    ///
    /// Creates the file if it doesn't exist (including parent directories).
    async fn open_file(&mut self, read_only: bool) -> Result<()> {
        self.ensure_parent_dirs()?;
        let mut opts = tokio::fs::OpenOptions::new();
        if read_only {
            opts.read(true);
        } else {
            opts.write(true).read(true).create(true);
        }
        let f = opts
            .open(&self.file_entry.path)
            .await
            .map_err(|e| Aria2Error::Io(format!("openFile {:?}: {}", self.file_entry.path, e)))?;
        self.file = Some(f);
        self.is_open = true;
        debug!("openFile: {:?}", self.file_entry.path);
        Ok(())
    }

    /// Open an existing file (fail if it doesn't exist).
    ///
    /// Does NOT create the file or parent directories.
    async fn open_existing_file(&mut self, read_only: bool) -> Result<()> {
        let mut opts = tokio::fs::OpenOptions::new();
        if read_only {
            opts.read(true);
        } else {
            opts.write(true).read(true);
        }
        let f = opts
            .open(&self.file_entry.path)
            .await
            .map_err(|e| Aria2Error::Io(format!("openExistingFile {:?}: {}", self.file_entry.path, e)))?;
        self.file = Some(f);
        self.is_open = true;
        debug!("openExistingFile: {:?}", self.file_entry.path);
        Ok(())
    }

    /// Open the file according to the given [`OpenMode`].
    async fn open_with_mode(&mut self, mode: OpenMode, read_only: bool) -> Result<()> {
        match mode {
            OpenMode::InitAndOpen => self.init_and_open_file(read_only).await,
            OpenMode::Open => self.open_file(read_only).await,
            OpenMode::OpenExisting => self.open_existing_file(read_only).await,
        }
    }

    /// Close the file handle if open.
    async fn close_file(&mut self) {
        if self.is_open {
            // Convert to std::fs::File and drop synchronously to avoid
            // Windows "Access denied" from background close task.
            if let Some(f) = self.file.take() {
                drop(f.into_std().await);
            }
            self.is_open = false;
            trace!("closeFile: {:?}", self.file_entry.path);
        }
    }

    /// Whether the file handle is currently open.
    pub fn is_open(&self) -> bool {
        self.is_open
    }

    /// Whether the file exists on disk.
    pub fn file_exists(&self) -> bool {
        self.file_entry.exists()
    }

    /// Actual size of the file on disk.
    pub async fn size(&self) -> Result<u64> {
        let meta = tokio::fs::metadata(&self.file_entry.path)
            .await
            .map_err(|e| Aria2Error::Io(format!("metadata {:?}: {}", self.file_entry.path, e)))?;
        Ok(meta.len())
    }

    /// Write `data` at `offset` within this file.
    ///
    /// The file must already be open.
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};
        let file = self.file.as_mut().ok_or_else(|| {
            Aria2Error::Io(format!(
                "write_at: file not open: {:?}",
                self.file_entry.path
            ))
        })?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| Aria2Error::Io(format!("seek {:?}: {}", self.file_entry.path, e)))?;
        file.write_all(data)
            .await
            .map_err(|e| Aria2Error::Io(format!("write {:?}: {}", self.file_entry.path, e)))?;
        Ok(())
    }

    /// Read exactly `buf.len()` bytes from `offset` within this file.
    ///
    /// Returns the number of bytes actually read (may be less than `buf.len()`
    /// at EOF). The file must already be open.
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let file = self.file.as_mut().ok_or_else(|| {
            Aria2Error::Io(format!(
                "read_at: file not open: {:?}",
                self.file_entry.path
            ))
        })?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| Aria2Error::Io(format!("seek {:?}: {}", self.file_entry.path, e)))?;
        match file.read(buf).await {
            Ok(0) => Ok(0),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Partial read at end of file — return what we got.
                Ok(0)
            }
            Err(e) => Err(Aria2Error::Io(format!(
                "read {:?}: {}",
                self.file_entry.path, e
            ))),
        }
    }

    /// Truncate this file to `length` bytes.
    async fn truncate(&mut self, length: u64) -> Result<()> {
        if let Some(ref mut file) = self.file {
            file.set_len(length)
                .await
                .map_err(|e| Aria2Error::Io(format!("truncate {:?}: {}", self.file_entry.path, e)))?;
        }
        Ok(())
    }

    /// Flush OS buffers for this file.
    async fn flush(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        if let Some(ref mut file) = self.file {
            file.flush()
                .await
                .map_err(|e| Aria2Error::Io(format!("flush {:?}: {}", self.file_entry.path, e)))?;
        }
        Ok(())
    }

    /// Whether this entry needs file allocation (pre-sizing on disk).
    pub fn needs_file_allocation(&self) -> bool {
        self.needs_file_allocation
    }

    /// Set the file allocation flag.
    pub fn set_needs_file_allocation(&mut self, flag: bool) {
        self.needs_file_allocation = flag;
    }

    /// Whether this non-requested file shares a piece with a requested file.
    pub fn needs_disk_writer(&self) -> bool {
        self.needs_disk_writer
    }

    /// Set the needs-disk-writer flag.
    pub fn set_needs_disk_writer(&mut self, flag: bool) {
        self.needs_disk_writer = flag;
    }

    /// Whether this entry has a disk writer (file handle or is eligible).
    pub fn has_disk_writer(&self) -> bool {
        // In C++, diskWriter_ being non-null indicates a writer was created.
        // In Rust, we consider an entry as having a disk writer if it's
        // eligible for I/O (needs_file_allocation || needs_disk_writer ||
        // file_exists).
        self.needs_file_allocation || self.needs_disk_writer || self.file_exists()
    }

    /// Create parent directories for the file path if they don't exist.
    fn ensure_parent_dirs(&self) -> Result<()> {
        if let Some(parent) = self.file_entry.path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Io(format!(
                    "create_dir_all {:?}: {}",
                    parent, e
                ))
            })?;
            debug!("Created parent directories: {:?}", parent);
        }
        Ok(())
    }
}

// =========================================================================
// MultiDiskAdaptor
// =========================================================================

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
    opened_entries: Vec<usize>,
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
                        warn!("Failed to get metadata for {:?}: {}", entry.file_entry.get_path(), e);
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
    fn find_first_entry_index(&self, offset: u64) -> Result<usize> {
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

// =========================================================================
// Unit tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── Helper: create temp file entries ─────────────────────────────

    fn make_test_entries(dir: &Path) -> Vec<FileEntry> {
        // C++ test layout (piece_length=2):
        //           1    1    2    2    3
        // 0....5....0....5....0....5....0
        // ++--++--++--++--++--++--++--++--
        // | file0 (len=0, off=0)
        // *************** file1 (len=15, off=0)
        //                ******* file2 (len=7, off=15)
        //                       | file3 (len=0, off=22)
        //                       ** file4 (len=2, off=22)
        //                         | file5 (len=0, off=24)
        //                         *** file6 (len=3, off=24)
        //                            | file7 (len=0, off=27)
        //                            ** file8 (len=2, off=27)
        vec![
            FileEntry::new(dir.join("file0.txt"), 0, 0, true),
            FileEntry::new(dir.join("file1.txt"), 15, 0, true),
            FileEntry::new(dir.join("file2.txt"), 7, 15, true),
            FileEntry::new(dir.join("file3.txt"), 0, 22, true),
            FileEntry::new(dir.join("file4.txt"), 2, 22, true),
            FileEntry::new(dir.join("file5.txt"), 0, 24, true),
            FileEntry::new(dir.join("file6.txt"), 3, 24, true),
            FileEntry::new(dir.join("file7.txt"), 0, 27, true),
            FileEntry::new(dir.join("file8.txt"), 2, 27, true),
        ]
    }

    // ── FileEntry tests ──────────────────────────────────────────────

    #[test]
    fn test_file_entry_basic() {
        let fe = FileEntry::new(PathBuf::from("/tmp/test.txt"), 1024, 2048, true);
        assert_eq!(fe.get_path(), Path::new("/tmp/test.txt"));
        assert_eq!(fe.get_length(), 1024);
        assert_eq!(fe.get_offset(), 2048);
        assert_eq!(fe.get_last_offset(), 3072);
        assert!(fe.is_requested());
    }

    #[test]
    fn test_file_entry_not_requested() {
        let fe = FileEntry::new(PathBuf::from("/tmp/skip.txt"), 100, 0, false);
        assert!(!fe.is_requested());
    }

    #[test]
    fn test_file_entry_zero_length() {
        let fe = FileEntry::new(PathBuf::from("/tmp/empty.txt"), 0, 50, true);
        assert_eq!(fe.get_length(), 0);
        assert_eq!(fe.get_offset(), 50);
        assert_eq!(fe.get_last_offset(), 50);
    }

    #[test]
    fn test_file_entry_set_requested() {
        let mut fe = FileEntry::new(PathBuf::from("/tmp/test.txt"), 100, 0, true);
        fe.set_requested(false);
        assert!(!fe.is_requested());
    }

    // ── Construction & shared-piece analysis ─────────────────────────

    #[test]
    fn test_construction_with_piece_length() {
        let adaptor = MultiDiskAdaptor::new(512);
        assert_eq!(adaptor.get_piece_length(), 512);
        assert!(!adaptor.is_read_only_enabled());
        assert!(adaptor.get_disk_writer_entries().is_empty());
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_all_requested() {
        let dir = tempfile::tempdir().unwrap();
        let entries = make_test_entries(dir.path());

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        // All entries should have disk writers since all are requested
        // (or are zero-length at the same offset as a requested file).
        assert!(dw_entries[0].has_disk_writer());
        assert!(dw_entries[1].has_disk_writer());
        assert!(dw_entries[2].has_disk_writer());
        assert!(dw_entries[3].has_disk_writer());
        assert!(dw_entries[4].has_disk_writer());
        assert!(dw_entries[5].has_disk_writer());

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_file0_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = make_test_entries(dir.path());
        entries[0].set_requested(false);

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        // file0 is not requested but file1 shares a piece with it.
        assert!(dw_entries[0].has_disk_writer());
        assert!(dw_entries[1].has_disk_writer());

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_file0_file1_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = make_test_entries(dir.path());
        entries[0].set_requested(false);
        entries[1].set_requested(false);

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        // file0: not requested, no piece sharing -> no disk writer
        assert!(!dw_entries[0].has_disk_writer());
        // file1: not requested but file2 spans into it -> needs file allocation
        assert!(dw_entries[1].has_disk_writer());
        assert!(dw_entries[1].needs_file_allocation());
        assert!(dw_entries[2].has_disk_writer());

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_file3_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = make_test_entries(dir.path());
        entries[3].set_requested(false);

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        // file3 not requested but file4 spans -> needs file allocation
        assert!(dw_entries[3].has_disk_writer());
        assert!(dw_entries[3].needs_file_allocation());
        assert!(dw_entries[4].has_disk_writer());

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_file4_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = make_test_entries(dir.path());
        entries[4].set_requested(false);

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        // file3 is zero-length, no overlap with file4
        assert!(!dw_entries[4].has_disk_writer());
        assert!(dw_entries[5].has_disk_writer());

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_file3_file4_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = make_test_entries(dir.path());
        entries[3].set_requested(false);
        entries[4].set_requested(false);

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        assert!(!dw_entries[3].has_disk_writer());
        assert!(!dw_entries[4].has_disk_writer());
        assert!(dw_entries[5].has_disk_writer());

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_only_first_requested() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = make_test_entries(dir.path());
        for i in 1..9 {
            entries[i].set_requested(false);
        }

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        assert!(dw_entries[0].has_disk_writer());
        assert!(!dw_entries[1].has_disk_writer());
        assert!(!dw_entries[2].has_disk_writer());
        assert!(!dw_entries[3].has_disk_writer());
        assert!(!dw_entries[4].has_disk_writer());
        assert!(!dw_entries[5].has_disk_writer());

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_first_two_requested() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = make_test_entries(dir.path());
        for i in 2..9 {
            entries[i].set_requested(false);
        }

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        assert!(dw_entries[0].has_disk_writer());
        assert!(dw_entries[1].has_disk_writer());
        // file1 spans into file2
        assert!(dw_entries[2].has_disk_writer());
        assert!(!dw_entries[2].needs_file_allocation());
        assert!(!dw_entries[3].has_disk_writer());
        assert!(!dw_entries[4].has_disk_writer());
        assert!(!dw_entries[5].has_disk_writer());

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_reset_disk_writer_entries_only_file6_requested() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = make_test_entries(dir.path());
        for i in 0..6 {
            entries[i].set_requested(false);
        }
        entries[8].set_requested(false);

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let dw_entries = adaptor.get_disk_writer_entries();
        assert!(!dw_entries[0].has_disk_writer());
        assert!(!dw_entries[1].has_disk_writer());
        assert!(!dw_entries[2].has_disk_writer());
        assert!(!dw_entries[3].has_disk_writer());
        assert!(!dw_entries[4].has_disk_writer());
        // file6 spans file5 (backward scan) and file8 (forward scan)
        assert!(dw_entries[5].has_disk_writer());
        assert!(dw_entries[6].has_disk_writer());
        assert!(dw_entries[7].has_disk_writer());
        assert!(dw_entries[8].has_disk_writer());

        adaptor.close_file().await;
    }

    // ── Binary search tests ──────────────────────────────────────────

    #[test]
    fn test_find_first_entry_index_basic() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("a"), 10, 0, true),
            FileEntry::new(dir.path().join("b"), 10, 10, true),
            FileEntry::new(dir.path().join("c"), 10, 20, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);

        assert_eq!(adaptor.find_first_entry_index(0).unwrap(), 0);
        assert_eq!(adaptor.find_first_entry_index(5).unwrap(), 0);
        assert_eq!(adaptor.find_first_entry_index(9).unwrap(), 0);
        assert_eq!(adaptor.find_first_entry_index(10).unwrap(), 1);
        assert_eq!(adaptor.find_first_entry_index(15).unwrap(), 1);
        assert_eq!(adaptor.find_first_entry_index(19).unwrap(), 1);
        assert_eq!(adaptor.find_first_entry_index(20).unwrap(), 2);
        assert_eq!(adaptor.find_first_entry_index(29).unwrap(), 2);
    }

    #[test]
    fn test_find_first_entry_index_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("a"), 10, 0, true),
            FileEntry::new(dir.path().join("b"), 10, 10, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);

        // Past the end of all files
        assert!(adaptor.find_first_entry_index(20).is_err());
    }

    #[test]
    fn test_find_first_entry_index_empty() {
        let adaptor = MultiDiskAdaptor::new(1);
        assert!(adaptor.find_first_entry_index(0).is_err());
    }

    #[test]
    fn test_find_first_entry_index_zero_length_files() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("a"), 0, 0, true),
            FileEntry::new(dir.path().join("b"), 5, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);

        // offset 0 falls in file b (len=5, off=0), not file a (len=0)
        assert_eq!(adaptor.find_first_entry_index(0).unwrap(), 1);
        assert_eq!(adaptor.find_first_entry_index(4).unwrap(), 1);
        assert!(adaptor.find_first_entry_index(5).is_err());
    }

    // ── Cross-file write tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_cross_file_write_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Write "12345" at offset 0 (within file1 only)
        adaptor.write_data(0, b"12345").await.unwrap();

        adaptor.close_file().await;

        // Verify file1.txt contains "12345"
        let content = tokio::fs::read_to_string(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(&content[..5], "12345");
    }

    #[tokio::test]
    async fn test_cross_file_write_spanning_two_files() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Write at offset 5, length 11: spans file1[5..15] and file2[0..1]
        adaptor.write_data(5, b"67890ABCDEF").await.unwrap();

        adaptor.close_file().await;

        // file1.txt should have 15 bytes (first 5 uninitialized, next 10 from write)
        let content1 = tokio::fs::read(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(content1.len(), 15);
        assert_eq!(&content1[5..15], b"67890ABCDE");

        // file2.txt first byte should be 'F'
        let content2 = tokio::fs::read(dir.path().join("file2.txt"))
            .await
            .unwrap();
        assert!(content2.len() >= 1);
        assert_eq!(content2[0], b'F');
    }

    #[tokio::test]
    async fn test_cross_file_write_full_cplusplus_test() {
        // Reproduces the C++ testWriteData test case.
        let dir = tempfile::tempdir().unwrap();
        let entries = make_test_entries(dir.path());

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Write "12345" at offset 0
        adaptor.write_data(0, b"12345").await.unwrap();
        adaptor.close_file().await;

        // Verify file1.txt
        let content1 = tokio::fs::read(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(&content1[..5], b"12345");

        // Re-open and write "67890ABCDEF" at offset 5
        adaptor.open_file().await.unwrap();
        adaptor.write_data(5, b"67890ABCDEF").await.unwrap();
        adaptor.close_file().await;

        let content1 = tokio::fs::read(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(content1.len(), 15);
        assert_eq!(&content1[..15], b"1234567890ABCDE");

        let content2 = tokio::fs::read(dir.path().join("file2.txt"))
            .await
            .unwrap();
        assert!(content2.len() >= 1);
        assert_eq!(content2[0], b'F');

        // Re-open and write "12345123456712" at offset 10
        adaptor.open_file().await.unwrap();
        adaptor.write_data(10, b"12345123456712").await.unwrap();
        adaptor.close_file().await;

        let content1 = tokio::fs::read(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(&content1[..15], b"123456789012345");

        let content2 = tokio::fs::read(dir.path().join("file2.txt"))
            .await
            .unwrap();
        assert_eq!(content2.len(), 7);
        assert_eq!(&content2[..7], b"1234567");
    }

    // ── Cross-file read tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_cross_file_read_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
        ];

        // Write test data first
        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        adaptor.write_data(0, b"1234567890ABCDE").await.unwrap();
        adaptor.write_data(15, b"FGHIJKLM").await.unwrap();
        adaptor.flush_os_buffers().await.unwrap();

        // Read from offset 0, length 15 (within file1)
        let data = adaptor.read_data(0, 15).await.unwrap();
        assert_eq!(&data[..15], b"1234567890ABCDE");

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_cross_file_read_spanning_files() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        adaptor.write_data(0, b"1234567890ABCDE").await.unwrap();
        adaptor.write_data(15, b"FGHIJKLM").await.unwrap();
        adaptor.flush_os_buffers().await.unwrap();

        // Read from offset 6, length 10: spans file1[6..15] and file2[0..1]
        let data = adaptor.read_data(6, 10).await.unwrap();
        assert_eq!(&data[..10], b"7890ABCDEF");

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_cross_file_read_across_three_files() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
            FileEntry::new(dir.path().join("file3.txt"), 3, 22, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Write data across all three files in one operation.
        // 25 bytes total: file1[0..15] + file2[0..7] + file3[0..3]
        adaptor
            .write_data(0, b"1234567890ABCDEFGHIJKLNOP")
            .await
            .unwrap();
        adaptor.flush_os_buffers().await.unwrap();

        // Read from offset 20, length 4: spans file2[5..7] and file3[0..2]
        // file2 = "FGHIJKL" -> file2[5..7] = "KL"
        // file3 = "NOP" -> file3[0..2] = "NO"
        let data = adaptor.read_data(20, 4).await.unwrap();
        assert_eq!(&data[..4], b"KLNO");

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_cross_file_read_full_stream() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
            FileEntry::new(dir.path().join("file3.txt"), 3, 22, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Write 25 bytes across all files in one operation:
        // file1[0..15] = "1234567890ABCDE"
        // file2[0..7]  = "FGHIJKL"
        // file3[0..3]  = "NOP"
        adaptor
            .write_data(0, b"1234567890ABCDEFGHIJKLNOP")
            .await
            .unwrap();
        adaptor.flush_os_buffers().await.unwrap();

        // Read entire stream (25 bytes)
        let data = adaptor.read_data(0, 25).await.unwrap();
        assert_eq!(&data[..25], b"1234567890ABCDEFGHIJKLNOP");

        adaptor.close_file().await;
    }

    // ── Lazy file opening tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_open_existing_file_lazy() {
        let dir = tempfile::tempdir().unwrap();

        // Create files on disk first
        tokio::fs::write(dir.path().join("file1.txt"), b"existing data here!")
            .await
            .unwrap();

        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 19, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(2);
        adaptor.set_file_entries(entries);
        adaptor.open_existing_file().await.unwrap();

        // No files should be open yet
        assert!(adaptor.opened_entries.is_empty());

        // But reading should work (lazy open)
        // "existing" is 8 bytes
        let data = adaptor.read_data(0, 8).await.unwrap();
        assert_eq!(&data[..8], b"existing");

        adaptor.close_file().await;
    }

    // ── File existence and size tests ────────────────────────────────

    #[tokio::test]
    async fn test_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("file1.txt"), b"hello")
            .await
            .unwrap();

        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 5, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 5, 5, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);

        // file1 exists, file2 doesn't
        assert!(adaptor.file_exists());
    }

    #[tokio::test]
    async fn test_size_calculation() {
        let dir = tempfile::tempdir().unwrap();

        // Create files with known sizes
        tokio::fs::write(dir.path().join("file1.txt"), vec![0u8; 256])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("file2.txt"), vec![0u8; 512])
            .await
            .unwrap();

        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 256, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 512, 256, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        let size = adaptor.size().await.unwrap();
        assert_eq!(size, 768);

        adaptor.close_file().await;
    }

    // ── cutTrailingGarbage tests ─────────────────────────────────────

    #[tokio::test]
    async fn test_cut_trailing_garbage() {
        let dir = tempfile::tempdir().unwrap();

        // Create files that are 100 bytes larger than declared
        tokio::fs::write(dir.path().join("file1.txt"), vec![0u8; 356])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("file2.txt"), vec![0u8; 612])
            .await
            .unwrap();

        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 256, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 512, 256, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        adaptor.cut_trailing_garbage().await.unwrap();

        // Verify files are truncated to declared lengths
        let meta1 = tokio::fs::metadata(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(meta1.len(), 256);

        let meta2 = tokio::fs::metadata(dir.path().join("file2.txt"))
            .await
            .unwrap();
        assert_eq!(meta2.len(), 512);

        adaptor.close_file().await;
    }

    // ── tryCloseFile eviction tests ──────────────────────────────────

    #[tokio::test]
    async fn test_try_close_file() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 10, 10, true),
            FileEntry::new(dir.path().join("file3.txt"), 10, 20, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // All 3 files should be open
        assert_eq!(adaptor.opened_entries.len(), 3);

        // Close 2 files
        let closed = adaptor.try_close_file(2).await;
        assert_eq!(closed, 2);
        assert_eq!(adaptor.opened_entries.len(), 1);

        // Close more than available
        let closed = adaptor.try_close_file(5).await;
        assert_eq!(closed, 1); // Only 1 was left
        assert!(adaptor.opened_entries.is_empty());
    }

    // ── Read-only mode tests ─────────────────────────────────────────

    #[test]
    fn test_read_only_toggle() {
        let mut adaptor = MultiDiskAdaptor::new(512);
        assert!(!adaptor.is_read_only_enabled());

        adaptor.enable_read_only();
        assert!(adaptor.is_read_only_enabled());

        adaptor.disable_read_only();
        assert!(!adaptor.is_read_only_enabled());
    }

    // ── Close all files tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_close_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 10, 10, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        assert!(!adaptor.opened_entries.is_empty());

        adaptor.close_file().await;
        assert!(adaptor.opened_entries.is_empty());
    }

    // ── Open mode strategy tests ─────────────────────────────────────

    #[tokio::test]
    async fn test_init_and_open_truncates() {
        let dir = tempfile::tempdir().unwrap();

        // Create file with existing content
        tokio::fs::write(dir.path().join("file1.txt"), b"existing content here")
            .await
            .unwrap();

        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 5, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.init_and_open_file().await.unwrap();

        // Write new data
        adaptor.write_data(0, b"12345").await.unwrap();
        adaptor.close_file().await;

        // File should contain only the new data (truncated)
        let content = tokio::fs::read(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(&content[..5], b"12345");
    }

    #[tokio::test]
    async fn test_open_file_preserves_content() {
        let dir = tempfile::tempdir().unwrap();

        // Create file with existing content (21 bytes)
        tokio::fs::write(dir.path().join("file1.txt"), b"existing content here")
            .await
            .unwrap();

        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 21, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Content should be preserved
        let data = adaptor.read_data(0, 21).await.unwrap();
        assert_eq!(&data[..21], b"existing content here");

        adaptor.close_file().await;
    }

    // ── DiskAdaptor trait tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_disk_adaptor_trait_open() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);

        // DiskAdaptor::open delegates to open_file
        adaptor.open(Path::new("ignored")).await.unwrap();

        adaptor.write(0, b"test data!").await.unwrap();
        adaptor.close().await.unwrap();

        let content = tokio::fs::read_to_string(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert!(content.starts_with("test data!"));
    }

    #[tokio::test]
    async fn test_disk_adaptor_trait_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
            FileEntry::new(dir.path().join("file2.txt"), 10, 15, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open(Path::new("ignored")).await.unwrap();

        // Write 16 bytes starting at offset 5: file1[5..15] (10 bytes) + file2[0..6] (6 bytes)
        adaptor.write(5, b"1234567890ABCDEF").await.unwrap();
        adaptor.flush().await.unwrap();

        // Read back via trait method
        let data = adaptor.read(5, 16).await.unwrap();
        assert_eq!(&data[..16], b"1234567890ABCDEF");

        adaptor.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_disk_adaptor_trait_truncate_error() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open(Path::new("ignored")).await.unwrap();

        // truncate should return error for MultiDiskAdaptor
        assert!(adaptor.truncate(100).await.is_err());

        adaptor.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_disk_adaptor_trait_size() {
        let dir = tempfile::tempdir().unwrap();

        tokio::fs::write(dir.path().join("file1.txt"), vec![0u8; 10])
            .await
            .unwrap();

        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open(Path::new("ignored")).await.unwrap();

        let size = adaptor.size().await.unwrap();
        assert_eq!(size, 10);

        adaptor.close().await.unwrap();
    }

    // ── Max open files eviction test ─────────────────────────────────

    #[tokio::test]
    async fn test_max_open_files_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let entries: Vec<FileEntry> = (0..5)
            .map(|i| FileEntry::new(dir.path().join(format!("file{}.txt", i)), 10, i * 10, true))
            .collect();

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_max_open_files(3);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Only 3 should be open (5 files, max 3)
        assert!(adaptor.opened_entries.len() <= 3);

        // Writing should still work even with eviction
        adaptor.write_data(0, b"0123456789").await.unwrap();
        adaptor.write_data(40, b"0123456789").await.unwrap();

        adaptor.close_file().await;
    }

    // ── Empty data edge cases ────────────────────────────────────────

    #[tokio::test]
    async fn test_write_empty_data() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Writing empty data should succeed without error
        adaptor.write_data(0, b"").await.unwrap();

        adaptor.close_file().await;
    }

    #[tokio::test]
    async fn test_read_zero_length() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        // Reading zero bytes should return empty vec
        let data = adaptor.read_data(0, 0).await.unwrap();
        assert!(data.is_empty());

        adaptor.close_file().await;
    }

    // ── Flush OS buffers test ────────────────────────────────────────

    #[tokio::test]
    async fn test_flush_os_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);
        adaptor.open_file().await.unwrap();

        adaptor.write_data(0, b"0123456789").await.unwrap();
        adaptor.flush_os_buffers().await.unwrap();

        adaptor.close_file().await;

        // Verify data persisted after flush
        let content = tokio::fs::read(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(&content[..10], b"0123456789");
    }

    // ── Multiple open/close cycles ───────────────────────────────────

    #[tokio::test]
    async fn test_open_close_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        ];

        let mut adaptor = MultiDiskAdaptor::new(1);
        adaptor.set_file_entries(entries);

        for i in 0..3 {
            adaptor.open_file().await.unwrap();
            adaptor.write_data(0, &[i; 10]).await.unwrap();
            adaptor.close_file().await;
        }

        // Last write should have written 0x02
        let content = tokio::fs::read(dir.path().join("file1.txt"))
            .await
            .unwrap();
        assert_eq!(content[..10], [2u8; 10]);
    }
}
