//! Shared SFTP file-operation values.

use super::super::packet::{
    SSH_FILEXFER_ATTR_ACMODTIME, SSH_FILEXFER_ATTR_PERMISSIONS, SSH_FILEXFER_ATTR_SIZE,
    SSH_FILEXFER_ATTR_UIDGID, SSH_FX_CONNECTION_LOST, SSH_FX_NO_CONNECTION, SSH_FX_NO_SUCH_FILE,
    SSH_FX_PERMISSION_DENIED, SSH_FXF_APPEND, SSH_FXF_CREAT, SSH_FXF_EXCL, SSH_FXF_READ,
    SSH_FXF_TRUNC, SSH_FXF_WRITE, SftpFileAttrs,
};
use super::super::session::SftpSessionError;
// FileOpError -- classified SFTP file operation error
// =============================================================================

/// Classified SFTP file operation error.
///
/// SFTP v3 status codes: `SSH_FX_NO_SUCH_FILE=2`,
/// `SSH_FX_PERMISSION_DENIED=3`, `SSH_FX_NO_CONNECTION=6`,
/// `SSH_FX_CONNECTION_LOST=7`.
#[derive(Debug)]
pub enum FileOpError {
    /// SSH_FX_NO_SUCH_FILE (code=2)
    NotFound { path: String },
    /// SSH_FX_PERMISSION_DENIED (code=3)
    PermissionDenied { path: String },
    /// SSH_FX_NO_CONNECTION (6) or SSH_FX_CONNECTION_LOST (7)
    Network { operation: String, message: String },
    /// A failure in the SFTP session used to execute this operation.
    Session {
        operation: &'static str,
        source: SftpSessionError,
    },
    /// All other errors
    Other { message: String },
}

impl FileOpError {
    pub(super) fn other(message: String) -> Self {
        Self::Other { message }
    }

    pub(super) fn session(operation: &'static str, source: SftpSessionError) -> Self {
        Self::Session { operation, source }
    }

    pub(super) fn from_status(
        operation: &'static str,
        code: u32,
        message: String,
        path: Option<&str>,
    ) -> Self {
        match code {
            SSH_FX_NO_SUCH_FILE => Self::NotFound {
                path: path.unwrap_or_default().to_string(),
            },
            SSH_FX_PERMISSION_DENIED => Self::PermissionDenied {
                path: path.unwrap_or_default().to_string(),
            },
            SSH_FX_NO_CONNECTION | SSH_FX_CONNECTION_LOST => Self::Network {
                operation: operation.to_string(),
                message,
            },
            _ => Self::Other {
                message: format!("{operation} failed (code={code}): {message}"),
            },
        }
    }
}

impl std::fmt::Display for FileOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileOpError::NotFound { path } if path.is_empty() => write!(f, "File not found"),
            FileOpError::NotFound { path } => write!(f, "File not found: {}", path),
            FileOpError::PermissionDenied { path } if path.is_empty() => {
                write!(f, "Permission denied")
            }
            FileOpError::PermissionDenied { path } => write!(f, "Permission denied: {}", path),
            FileOpError::Network { operation, message } => {
                write!(f, "{} failed: {}", operation, message)
            }
            FileOpError::Session { operation, source } => {
                write!(f, "{} request failed: {}", operation, source)
            }
            FileOpError::Other { message } => write!(f, "{}", message),
        }
    }
}

impl std::error::Error for FileOpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileOpError::Session { source, .. } => Some(source),
            _ => None,
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
            flags |= SSH_FILEXFER_ATTR_SIZE;
        }
        if self.uid != 0 || self.gid != 0 {
            flags |= SSH_FILEXFER_ATTR_UIDGID;
        }
        if self.permissions != 0 {
            flags |= SSH_FILEXFER_ATTR_PERMISSIONS;
        }
        if self.atime != 0 || self.mtime != 0 {
            flags |= SSH_FILEXFER_ATTR_ACMODTIME;
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
