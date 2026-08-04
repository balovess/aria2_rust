//! Disk writer entry that associates a [`FileEntry`] with an optional file
//! handle and I/O flags.

use std::path::Path;

use tracing::{debug, trace};

use crate::error::{Aria2Error, Result};

use super::file_entry::FileEntry;

// =========================================================================
// OpenMode
// =========================================================================

/// Strategy for opening a file within a `DiskWriterEntry`.
///
/// Replaces the C++ member-function-pointer pattern used in
/// `MultiDiskAdaptor::openIfNot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum OpenMode {
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
    pub(super) file_entry: FileEntry,
    file: Option<tokio::fs::File>,
    is_open: bool,
    pub(super) needs_file_allocation: bool,
    pub(super) needs_disk_writer: bool,
}

impl DiskWriterEntry {
    /// Create a new entry from a [`FileEntry`].
    ///
    /// The file is not opened; `is_open` starts as `false`.
    /// `needs_file_allocation` is initialized to `file_entry.is_requested()`,
    /// matching the C++ `createDiskWriterEntry` helper.
    pub(super) fn new(file_entry: FileEntry) -> Self {
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
    pub(super) async fn init_and_open_file(&mut self, read_only: bool) -> Result<()> {
        self.ensure_parent_dirs()?;
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(!read_only)
            .read(true)
            .create(true)
            .truncate(true);
        let f = opts.open(&self.file_entry.path).await.map_err(|e| {
            Aria2Error::FileCreate(format!("initAndOpenFile {:?}: {}", self.file_entry.path, e))
        })?;
        self.file = Some(f);
        self.is_open = true;
        debug!("initAndOpenFile: {:?}", self.file_entry.path);
        Ok(())
    }

    /// Open the file without truncation.
    ///
    /// Creates the file if it doesn't exist (including parent directories).
    pub(super) async fn open_file(&mut self, read_only: bool) -> Result<()> {
        self.ensure_parent_dirs()?;
        let mut opts = tokio::fs::OpenOptions::new();
        if read_only {
            opts.read(true);
        } else {
            opts.write(true).read(true).create(true);
        }
        let f = opts.open(&self.file_entry.path).await.map_err(|e| {
            Aria2Error::FileOpen(format!("openFile {:?}: {}", self.file_entry.path, e))
        })?;
        self.file = Some(f);
        self.is_open = true;
        debug!("openFile: {:?}", self.file_entry.path);
        Ok(())
    }

    /// Open an existing file (fail if it doesn't exist).
    ///
    /// Does NOT create the file or parent directories.
    pub(super) async fn open_existing_file(&mut self, read_only: bool) -> Result<()> {
        let mut opts = tokio::fs::OpenOptions::new();
        if read_only {
            opts.read(true);
        } else {
            opts.write(true).read(true);
        }
        let f = opts.open(&self.file_entry.path).await.map_err(|e| {
            Aria2Error::Io(format!(
                "openExistingFile {:?}: {}",
                self.file_entry.path, e
            ))
        })?;
        self.file = Some(f);
        self.is_open = true;
        debug!("openExistingFile: {:?}", self.file_entry.path);
        Ok(())
    }

    /// Open the file according to the given [`OpenMode`].
    pub(super) async fn open_with_mode(&mut self, mode: OpenMode, read_only: bool) -> Result<()> {
        match mode {
            OpenMode::InitAndOpen => self.init_and_open_file(read_only).await,
            OpenMode::Open => self.open_file(read_only).await,
            OpenMode::OpenExisting => self.open_existing_file(read_only).await,
        }
    }

    /// Close the file handle if open.
    pub(super) async fn close_file(&mut self) {
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
    pub(super) async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
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
    pub(super) async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
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
    pub(super) async fn truncate(&mut self, length: u64) -> Result<()> {
        if let Some(ref mut file) = self.file {
            file.set_len(length).await.map_err(|e| {
                Aria2Error::Io(format!("truncate {:?}: {}", self.file_entry.path, e))
            })?;
        }
        Ok(())
    }

    /// Flush OS buffers for this file.
    pub(super) async fn flush(&mut self) -> Result<()> {
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
                Aria2Error::DirCreate(format!("create_dir_all {:?}: {}", parent, e))
            })?;
            debug!("Created parent directories: {:?}", parent);
        }
        Ok(())
    }
}
