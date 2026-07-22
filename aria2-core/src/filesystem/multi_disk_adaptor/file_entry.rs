//! Per-file entry within a multi-file torrent/download.

use std::path::{Path, PathBuf};

/// Represents a single file within a multi-file torrent/download.
///
/// Each `FileEntry` describes a contiguous region of the global byte stream:
/// `[offset, offset + length)`. Files are sorted by offset and do not overlap.
///
/// This is the Rust equivalent of the C++ `FileEntry` used by
/// `MultiDiskAdaptor`.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub(super) path: PathBuf,
    pub(super) length: u64,
    pub(super) offset: u64,
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
