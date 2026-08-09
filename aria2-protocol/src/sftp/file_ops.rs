//! SFTP file operations and high-level file I/O abstractions.
//!
//! Provides `SftpFileOps` for issuing file system operations (open, close,
//! read, write, stat, etc.) over an active SFTP session, plus `SftpRemoteFile`
//! for streaming read/write access to a remote file handle.
//!
//! ## Architecture
//!
//! ```text
//! SftpFileOps  -- issues SftpPacket requests via SftpSession
//!      |
//!      +-- open()  --> SftpRemoteFile (holds open handle)
//!      |                     |
//!      |                     +-- read_at() / write_at() / close()
//!      |
//!      +-- stat() / lstat() / set_stat() / mkdir() / rmdir() / ...
//! ```
//!
//! All operations translate into the corresponding `SftpPacket` variants and
//! delegate to `SftpSession::request()` for send/receive with request ID
//! management.

use tracing::{debug, warn};

use super::packet::{
    SSH_FX_EOF, SSH_FX_OK, SSH_FXF_APPEND, SSH_FXF_CREAT, SSH_FXF_EXCL, SSH_FXF_READ,
    SSH_FXF_TRUNC, SSH_FXF_WRITE, SftpFileAttrs, SftpPacket,
};
use super::session::SftpSession;

// =============================================================================
// FileOpError -- classified SFTP file operation error
// =============================================================================

/// Classified SFTP file operation error.
///
/// Converted from the `String` errors returned by `SftpFileOps` methods,
/// which have the format: `"<Operation> failed (code=N): message"`.
/// The SFTP status code N determines the variant.
///
/// SFTP status codes (from `packet.rs`): `SSH_FX_NO_SUCH_FILE=2`,
/// `SSH_FX_PERMISSION_DENIED=3`, `SSH_FX_NO_CONNECTION=6`,
/// `SSH_FX_CONNECTION_LOST=7`.
#[derive(Debug, Clone)]
pub enum FileOpError {
    /// SSH_FX_NO_SUCH_FILE (code=2)
    NotFound { path: String },
    /// SSH_FX_PERMISSION_DENIED (code=3)
    PermissionDenied { path: String },
    /// SSH_FX_NO_CONNECTION (6) or SSH_FX_CONNECTION_LOST (7)
    Network { operation: String, message: String },
    /// All other errors
    Other { message: String },
}

impl From<String> for FileOpError {
    fn from(s: String) -> Self {
        // Error strings look like: "Open failed (code=2): No such file"
        // Extract the status code to classify.
        if let Some(code_start) = s.find("(code=") {
            let rest = &s[code_start + 6..];
            if let Some(paren_end) = rest.find(')')
                && let Ok(code) = rest[..paren_end].parse::<u32>()
            {
                // Extract operation name (text before " failed").
                let operation = s.split(" failed").next().unwrap_or("Unknown").to_string();
                // Extract message after "): ".
                let message = s.split("): ").nth(1).unwrap_or(&s).to_string();

                return match code {
                    2 => FileOpError::NotFound {
                        path: String::new(),
                    },
                    3 => FileOpError::PermissionDenied {
                        path: String::new(),
                    },
                    6 | 7 => FileOpError::Network { operation, message },
                    _ => FileOpError::Other { message: s.clone() },
                };
            }
        }
        FileOpError::Other { message: s }
    }
}

impl std::fmt::Display for FileOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileOpError::NotFound { path } => write!(f, "File not found: {}", path),
            FileOpError::PermissionDenied { path } => write!(f, "Permission denied: {}", path),
            FileOpError::Network { operation, message } => {
                write!(f, "{} failed: {}", operation, message)
            }
            FileOpError::Other { message } => write!(f, "{}", message),
        }
    }
}

// =============================================================================
// OpenFlags -- SFTP file open flags
// =============================================================================

/// SFTP file open flags (SSH_FXF_* bitmask).
///
/// These map directly to the SFTP v3 open flags defined in the protocol spec.
/// Use the convenience constructors `readonly()`, `write_create()`, etc. for
/// common patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenFlags(u32);

impl OpenFlags {
    /// Open for reading only: `SSH_FXF_READ`.
    pub fn readonly() -> Self {
        Self(SSH_FXF_READ)
    }

    /// Open for writing, create if missing, truncate: `WRITE | CREAT | TRUNC`.
    pub fn write_create() -> Self {
        Self(SSH_FXF_WRITE | SSH_FXF_CREAT | SSH_FXF_TRUNC)
    }

    /// Open for reading and writing.
    pub fn read_write() -> Self {
        Self(SSH_FXF_READ | SSH_FXF_WRITE)
    }

    /// Open for appending (implies write).
    pub fn append() -> Self {
        Self(SSH_FXF_WRITE | SSH_FXF_APPEND | SSH_FXF_CREAT)
    }

    /// Create a new file; fail if it already exists (`WRITE | CREAT | EXCL`).
    pub fn create_new() -> Self {
        Self(SSH_FXF_WRITE | SSH_FXF_CREAT | SSH_FXF_EXCL)
    }

    /// Create from a raw bitmask.
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Return the raw bitmask value.
    pub fn bits(&self) -> u32 {
        self.0
    }

    /// Check if READ flag is set.
    pub fn is_read(&self) -> bool {
        self.0 & SSH_FXF_READ != 0
    }

    /// Check if WRITE flag is set.
    pub fn is_write(&self) -> bool {
        self.0 & SSH_FXF_WRITE != 0
    }

    /// Check if APPEND flag is set.
    pub fn is_append(&self) -> bool {
        self.0 & SSH_FXF_APPEND != 0
    }

    /// Check if CREAT flag is set.
    pub fn is_create(&self) -> bool {
        self.0 & SSH_FXF_CREAT != 0
    }

    /// Check if TRUNC flag is set.
    pub fn is_trunc(&self) -> bool {
        self.0 & SSH_FXF_TRUNC != 0
    }

    /// Check if EXCL flag is set.
    pub fn is_excl(&self) -> bool {
        self.0 & SSH_FXF_EXCL != 0
    }
}

impl std::fmt::Display for OpenFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if self.is_read() {
            parts.push("READ");
        }
        if self.is_write() {
            parts.push("WRITE");
        }
        if self.is_append() {
            parts.push("APPEND");
        }
        if self.is_create() {
            parts.push("CREAT");
        }
        if self.is_trunc() {
            parts.push("TRUNC");
        }
        if self.is_excl() {
            parts.push("EXCL");
        }
        if parts.is_empty() {
            write!(f, "OPEN(0x{:08X})", self.0)
        } else {
            write!(f, "OPEN({})", parts.join("|"))
        }
    }
}

// =============================================================================
// FileAttributes -- high-level file attribute representation
// =============================================================================

/// High-level file attributes returned by stat/lstat operations.
///
/// Unlike `SftpFileAttrs` (which mirrors the wire format with optional fields
/// controlled by flags), this struct always populates every field with a
/// sensible default so callers do not need to check `flags` before accessing
/// size, permissions, etc.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileAttributes {
    /// File size in bytes (0 if unknown or not a regular file)
    pub size: u64,
    /// Owner user ID (0 if unknown)
    pub uid: u32,
    /// Owner group ID (0 if unknown)
    pub gid: u32,
    /// POSIX permission bits (0 if unknown)
    pub permissions: u32,
    /// Last access time as Unix timestamp (0 if unknown)
    pub atime: u32,
    /// Last modification time as Unix timestamp (0 if unknown)
    pub mtime: u32,
    /// True if this is a regular file
    pub is_regular_file: bool,
    /// True if this is a directory
    pub is_directory: bool,
    /// True if this is a symbolic link
    pub is_symlink: bool,
}

impl FileAttributes {
    /// Create a `FileAttributes` from the wire-format `SftpFileAttrs`.
    pub fn from_wire(wire: &SftpFileAttrs) -> Self {
        let permissions = wire.permissions.unwrap_or(0);
        Self {
            size: wire.size.unwrap_or(0),
            uid: wire.uid.unwrap_or(0),
            gid: wire.gid.unwrap_or(0),
            permissions,
            atime: wire.atime.unwrap_or(0),
            mtime: wire.mtime.unwrap_or(0),
            is_regular_file: wire.is_regular_file(),
            is_directory: wire.is_directory(),
            is_symlink: wire.is_symlink(),
        }
    }

    /// Convert back to the wire-format `SftpFileAttrs` for SETSTAT operations.
    pub fn to_wire(&self) -> SftpFileAttrs {
        let mut flags = 0;
        if self.size != 0 {
            flags |= super::packet::SSH_FILEXFER_ATTR_SIZE;
        }
        if self.uid != 0 || self.gid != 0 {
            flags |= super::packet::SSH_FILEXFER_ATTR_UIDGID;
        }
        if self.permissions != 0 {
            flags |= super::packet::SSH_FILEXFER_ATTR_PERMISSIONS;
        }
        if self.atime != 0 || self.mtime != 0 {
            flags |= super::packet::SSH_FILEXFER_ATTR_ACMODTIME;
        }
        SftpFileAttrs {
            flags,
            size: Some(self.size),
            uid: Some(self.uid),
            gid: Some(self.gid),
            permissions: Some(self.permissions),
            atime: Some(self.atime),
            mtime: Some(self.mtime),
        }
    }
}

impl std::fmt::Display for FileAttributes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = if self.is_regular_file {
            "file"
        } else if self.is_directory {
            "dir"
        } else if self.is_symlink {
            "symlink"
        } else {
            "other"
        };
        write!(
            f,
            "FileAttributes{{kind={}, size={}, perm=0o{:o}}}",
            kind, self.size, self.permissions
        )
    }
}

// =============================================================================
// SftpRemoteFile -- open file handle wrapper
// =============================================================================

/// An open remote file handle obtained via `SftpFileOps::open()`.
///
/// Provides streaming read/write access at arbitrary offsets. The handle is
/// automatically closed when dropped, but callers should prefer explicit
/// `close()` for error handling.
pub struct SftpRemoteFile<'a> {
    /// Reference to the session for issuing requests
    session: &'a SftpSession,
    /// The opaque file handle returned by the server (SSH_FXP_HANDLE)
    handle: Vec<u8>,
    /// Whether this handle has been closed
    closed: bool,
}

impl<'a> std::fmt::Debug for SftpRemoteFile<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpRemoteFile")
            .field("handle_len", &self.handle.len())
            .field("closed", &self.closed)
            .finish()
    }
}

impl<'a> SftpRemoteFile<'a> {
    fn new(session: &'a SftpSession, handle: Vec<u8>) -> Self {
        Self {
            session,
            handle,
            closed: false,
        }
    }

    /// Read up to `len` bytes starting at `offset` from the remote file.
    ///
    /// Returns the data bytes on success, or an empty Vec on EOF.
    pub async fn read_at(&self, offset: u64, len: u32) -> Result<Vec<u8>, String> {
        let pkt = SftpPacket::Read {
            request_id: 0, // Will be set by session.request()
            handle: self.handle.clone(),
            offset,
            length: len,
        };

        let resp = self.session.request(pkt).await?;

        match resp {
            SftpPacket::Data { data, .. } => Ok(data),
            SftpPacket::Status { code, .. } if code == SSH_FX_EOF => Ok(Vec::new()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Read failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to READ: type={}",
                other.packet_type()
            )),
        }
    }

    /// Write `data` starting at `offset` to the remote file.
    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String> {
        let pkt = SftpPacket::Write {
            request_id: 0,
            handle: self.handle.clone(),
            offset,
            data: data.to_vec(),
        };

        let resp = self.session.request(pkt).await?;

        match resp {
            SftpPacket::Status { code, .. } if code == SSH_FX_OK => Ok(()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Write failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to WRITE: type={}",
                other.packet_type()
            )),
        }
    }

    /// Close the remote file handle explicitly.
    ///
    /// Sends SSH_FXP_CLOSE and marks the handle as closed. Calling `close()`
    /// more than once is a no-op.
    pub async fn close(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }

        let pkt = SftpPacket::Close {
            request_id: 0,
            handle: self.handle.clone(),
        };

        let resp = self.session.request(pkt).await?;
        self.closed = true;

        match resp {
            SftpPacket::Status { code, .. } if code == SSH_FX_OK => Ok(()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Close failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to CLOSE: type={}",
                other.packet_type()
            )),
        }
    }
}

impl<'a> Drop for SftpRemoteFile<'a> {
    fn drop(&mut self) {
        if !self.closed {
            warn!(
                "[SFTP] SftpRemoteFile dropped without explicit close (handle_len={})",
                self.handle.len()
            );
            // Cannot await close() in drop; the handle will be orphaned on
            // the server side and eventually cleaned up when the session ends.
        }
    }
}

// =============================================================================
// SftpFileOps -- high-level file operation interface
// =============================================================================

/// High-level SFTP file operations bound to an active session.
///
/// Each method maps to one or more SFTP protocol packets and returns
/// ergonomic Rust types rather than raw protocol packets.
///
/// # Example
///
/// ```ignore
/// let session = SftpSession::open(&mut conn).await?;
/// let ops = SftpFileOps::new(&session);
///
/// let attr = ops.lstat("/remote/file.txt").await?;
/// println!("Size: {} bytes", attr.size);
///
/// let mut f = ops.open("/remote/file.txt", OpenFlags::readonly(), 0).await?;
/// let data = f.read_at(0, 4096).await?;
/// f.close().await?;
/// ```
pub struct SftpFileOps<'a> {
    session: &'a SftpSession,
}

impl<'a> std::fmt::Debug for SftpFileOps<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpFileOps").finish()
    }
}

impl<'a> SftpFileOps<'a> {
    /// Create a new file operations interface bound to the given session.
    pub fn new(session: &'a SftpSession) -> Self {
        Self { session }
    }

    // -----------------------------------------------------------------
    // File Open / Close
    // -----------------------------------------------------------------

    /// Open a remote file with the specified flags and initial attributes.
    ///
    /// Returns an `SftpRemoteFile` that supports streaming read/write at
    /// arbitrary offsets.
    pub async fn open(
        &self,
        path: &str,
        flags: OpenFlags,
        mode: u32,
    ) -> Result<SftpRemoteFile<'a>, String> {
        debug!("[SFTP] open({}, {})", path, flags);

        let attrs = SftpFileAttrs {
            flags: if mode != 0 {
                super::packet::SSH_FILEXFER_ATTR_PERMISSIONS
            } else {
                0
            },
            permissions: if mode != 0 { Some(mode) } else { None },
            ..Default::default()
        };

        let pkt = SftpPacket::Open {
            request_id: 0,
            filename: path.to_string(),
            flags: flags.bits(),
            attrs,
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Handle { handle, .. } => {
                debug!("[SFTP] open() got handle (len={})", handle.len());
                Ok(SftpRemoteFile::new(self.session, handle))
            }
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Open failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to OPEN: type={}",
                other.packet_type()
            )),
        }
    }

    // -----------------------------------------------------------------
    // Stat / Lstat
    // -----------------------------------------------------------------

    /// Get file attributes, following symlinks (SSH_FXP_STAT).
    pub async fn stat(&self, path: &str) -> Result<FileAttributes, String> {
        let pkt = SftpPacket::Stat {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Attrs { attrs, .. } => Ok(FileAttributes::from_wire(&attrs)),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Stat failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to STAT: type={}",
                other.packet_type()
            )),
        }
    }

    /// Get file attributes without following symlinks (SSH_FXP_LSTAT).
    pub async fn lstat(&self, path: &str) -> Result<FileAttributes, String> {
        let pkt = SftpPacket::Lstat {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Attrs { attrs, .. } => Ok(FileAttributes::from_wire(&attrs)),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Lstat failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to LSTAT: type={}",
                other.packet_type()
            )),
        }
    }

    // -----------------------------------------------------------------
    // Setstat
    // -----------------------------------------------------------------

    /// Set file attributes by path (SSH_FXP_SETSTAT).
    pub async fn set_stat(&self, path: &str, attrs: &FileAttributes) -> Result<(), String> {
        let pkt = SftpPacket::Setstat {
            request_id: 0,
            path: path.to_string(),
            attrs: attrs.to_wire(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Status { code, .. } if code == SSH_FX_OK => Ok(()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Setstat failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to SETSTAT: type={}",
                other.packet_type()
            )),
        }
    }

    // -----------------------------------------------------------------
    // Directory Operations
    // -----------------------------------------------------------------

    /// Open a directory for listing (SSH_FXP_OPENDIR).
    pub async fn opendir(&self, path: &str) -> Result<SftpRemoteFile<'a>, String> {
        let pkt = SftpPacket::Opendir {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Handle { handle, .. } => Ok(SftpRemoteFile::new(self.session, handle)),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Opendir failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to OPENDIR: type={}",
                other.packet_type()
            )),
        }
    }

    /// Read directory entries from an open directory handle (SSH_FXP_READDIR).
    ///
    /// Returns a list of `(filename, longname, FileAttributes)` tuples.
    /// An empty list indicates end-of-directory.
    pub async fn readdir(
        &self,
        dir_handle: &SftpRemoteFile<'_>,
    ) -> Result<Vec<(String, String, FileAttributes)>, String> {
        let pkt = SftpPacket::Readdir {
            request_id: 0,
            handle: dir_handle.handle.clone(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Name { entries, .. } => Ok(entries
                .into_iter()
                .map(|e| (e.filename, e.longname, FileAttributes::from_wire(&e.attrs)))
                .collect()),
            SftpPacket::Status { code, .. } if code == SSH_FX_EOF => Ok(Vec::new()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Readdir failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to READDIR: type={}",
                other.packet_type()
            )),
        }
    }

    /// Create a directory (SSH_FXP_MKDIR).
    pub async fn mkdir(&self, path: &str) -> Result<(), String> {
        let pkt = SftpPacket::Mkdir {
            request_id: 0,
            path: path.to_string(),
            attrs: SftpFileAttrs::default(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Status { code, .. } if code == SSH_FX_OK => Ok(()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Mkdir failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to MKDIR: type={}",
                other.packet_type()
            )),
        }
    }

    /// Remove a directory (SSH_FXP_RMDIR).
    pub async fn rmdir(&self, path: &str) -> Result<(), String> {
        let pkt = SftpPacket::Rmdir {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Status { code, .. } if code == SSH_FX_OK => Ok(()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Rmdir failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to RMDIR: type={}",
                other.packet_type()
            )),
        }
    }

    // -----------------------------------------------------------------
    // File Manipulation
    // -----------------------------------------------------------------

    /// Delete a file (SSH_FXP_REMOVE).
    pub async fn remove(&self, path: &str) -> Result<(), String> {
        let pkt = SftpPacket::Remove {
            request_id: 0,
            filename: path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Status { code, .. } if code == SSH_FX_OK => Ok(()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Remove failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to REMOVE: type={}",
                other.packet_type()
            )),
        }
    }

    /// Rename a file or directory (SSH_FXP_RENAME).
    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<(), String> {
        let pkt = SftpPacket::Rename {
            request_id: 0,
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Status { code, .. } if code == SSH_FX_OK => Ok(()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Rename failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to RENAME: type={}",
                other.packet_type()
            )),
        }
    }

    /// Canonicalize a path (SSH_FXP_REALPATH).
    pub async fn realpath(&self, path: &str) -> Result<String, String> {
        let pkt = SftpPacket::Realpath {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Name { mut entries, .. } => {
                if entries.is_empty() {
                    Err("REALPATH returned empty name list".to_string())
                } else {
                    Ok(entries.remove(0).filename)
                }
            }
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Realpath failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to REALPATH: type={}",
                other.packet_type()
            )),
        }
    }

    /// Read the target of a symbolic link (SSH_FXP_READLINK).
    pub async fn readlink(&self, path: &str) -> Result<String, String> {
        let pkt = SftpPacket::Readlink {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Name { mut entries, .. } => {
                if entries.is_empty() {
                    Err("READLINK returned empty name list".to_string())
                } else {
                    Ok(entries.remove(0).filename)
                }
            }
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Readlink failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to READLINK: type={}",
                other.packet_type()
            )),
        }
    }

    /// Create a symbolic link (SSH_FXP_SYMLINK).
    pub async fn symlink(&self, link_path: &str, target_path: &str) -> Result<(), String> {
        let pkt = SftpPacket::Symlink {
            request_id: 0,
            link_path: link_path.to_string(),
            target_path: target_path.to_string(),
        };

        let resp = self.session.request(pkt).await?;
        self.session.record_operation();

        match resp {
            SftpPacket::Status { code, .. } if code == SSH_FX_OK => Ok(()),
            SftpPacket::Status { code, message, .. } => {
                Err(format!("Symlink failed (code={}): {}", code, message))
            }
            other => Err(format!(
                "Unexpected response to SYMLINK: type={}",
                other.packet_type()
            )),
        }
    }
}

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_flags_readonly() {
        let flags = OpenFlags::readonly();
        assert!(flags.is_read());
        assert!(!flags.is_write());
        assert_eq!(flags.bits(), SSH_FXF_READ);
    }

    #[test]
    fn test_open_flags_write_create() {
        let flags = OpenFlags::write_create();
        assert!(!flags.is_read());
        assert!(flags.is_write());
        assert!(flags.is_create());
        assert!(flags.is_trunc());
        assert_eq!(flags.bits(), SSH_FXF_WRITE | SSH_FXF_CREAT | SSH_FXF_TRUNC);
    }

    #[test]
    fn test_open_flags_read_write() {
        let flags = OpenFlags::read_write();
        assert!(flags.is_read());
        assert!(flags.is_write());
        assert!(!flags.is_append());
        assert_eq!(flags.bits(), SSH_FXF_READ | SSH_FXF_WRITE);
    }

    #[test]
    fn test_open_flags_append() {
        let flags = OpenFlags::append();
        assert!(flags.is_write());
        assert!(flags.is_append());
        assert!(flags.is_create());
    }

    #[test]
    fn test_open_flags_create_new() {
        let flags = OpenFlags::create_new();
        assert!(flags.is_write());
        assert!(flags.is_create());
        assert!(flags.is_excl());
        assert!(!flags.is_trunc());
    }

    #[test]
    fn test_open_flags_from_bits() {
        let flags = OpenFlags::from_bits(0xFF);
        assert_eq!(flags.bits(), 0xFF);
    }

    #[test]
    fn test_open_flags_display() {
        assert_eq!(OpenFlags::readonly().to_string(), "OPEN(READ)");
        assert_eq!(
            OpenFlags::write_create().to_string(),
            "OPEN(WRITE|CREAT|TRUNC)"
        );
        assert_eq!(OpenFlags::from_bits(0).to_string(), "OPEN(0x00000000)");
    }

    #[test]
    fn test_file_attributes_default() {
        let attrs = FileAttributes::default();
        assert_eq!(attrs.size, 0);
        assert_eq!(attrs.permissions, 0);
        assert!(!attrs.is_regular_file);
        assert!(!attrs.is_directory);
        assert!(!attrs.is_symlink);
    }

    #[test]
    fn test_file_attributes_from_wire_regular_file() {
        let wire = SftpFileAttrs::full(12345, 1000, 1000, 0o100644, 1700000000, 1700000100);
        let attrs = FileAttributes::from_wire(&wire);
        assert_eq!(attrs.size, 12345);
        assert_eq!(attrs.uid, 1000);
        assert_eq!(attrs.gid, 1000);
        assert_eq!(attrs.permissions, 0o100644);
        assert!(attrs.is_regular_file);
        assert!(!attrs.is_directory);
        assert!(!attrs.is_symlink);
    }

    #[test]
    fn test_file_attributes_from_wire_directory() {
        let wire = SftpFileAttrs::full(4096, 0, 0, 0o040755, 0, 0);
        let attrs = FileAttributes::from_wire(&wire);
        assert!(attrs.is_directory);
        assert!(!attrs.is_regular_file);
    }

    #[test]
    fn test_file_attributes_from_wire_symlink() {
        let wire = SftpFileAttrs::full(10, 0, 0, 0o120777, 0, 0);
        let attrs = FileAttributes::from_wire(&wire);
        assert!(attrs.is_symlink);
        assert!(!attrs.is_regular_file);
        assert!(!attrs.is_directory);
    }

    #[test]
    fn test_file_attributes_to_wire_roundtrip() {
        let original = FileAttributes {
            size: 999,
            uid: 500,
            gid: 600,
            permissions: 0o100755,
            atime: 1700000000,
            mtime: 1700000100,
            is_regular_file: true,
            is_directory: false,
            is_symlink: false,
        };
        let wire = original.to_wire();
        let roundtrip = FileAttributes::from_wire(&wire);
        assert_eq!(roundtrip.size, original.size);
        assert_eq!(roundtrip.uid, original.uid);
        assert_eq!(roundtrip.gid, original.gid);
        assert_eq!(roundtrip.permissions, original.permissions);
        assert!(roundtrip.is_regular_file);
    }

    #[test]
    fn test_file_attributes_display() {
        let attrs = FileAttributes {
            size: 1024,
            permissions: 0o100644,
            is_regular_file: true,
            ..Default::default()
        };
        let display = attrs.to_string();
        assert!(display.contains("file"));
        assert!(display.contains("1024"));
    }

    #[test]
    fn test_file_attributes_permissions_only_roundtrip() {
        // This is the pattern used in transfer.rs for set_stat
        let attrs = FileAttributes {
            permissions: 0o644,
            ..Default::default()
        };
        let wire = attrs.to_wire();
        assert_eq!(wire.permissions, Some(0o644));
        assert_eq!(wire.size, Some(0)); // Default 0 is always populated in to_wire
    }
}
