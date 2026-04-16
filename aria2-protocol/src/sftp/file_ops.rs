//! SFTP File Operations
//!
//! Provides a high-level API for SFTP file and directory operations built entirely
//! on the pure Rust `packet.rs` codec and russh channel I/O. Maps SFTP protocol
//! errors to application-level error codes for consistent error handling.
//!
//! ## Operation Categories
//!
//! - **File operations**: open, close, read, write, stat, fstat
//! - **Directory operations**: opendir, readdir, mkdir, rmdir
//! - **Path operations**: realpath, rename, unlink, symlink, readlink
//! - **Attribute operations**: setstat, setfstat
//!
//! ## Architecture
//!
//! ```text
//! SftpFileOps  ->  SftpSession::request(SftpPacket)  ->  channel encode/send
//!       |                    |                              |
//!   High-level API    Request ID management           recv/decode response
//!       |                    |                              |
//!       v                    v                              v
//! FileAttributes     SftpPacket (request)          SftpPacket (response)
//! ```

use std::sync::Arc;
use tracing::{debug, warn};

use super::packet::{
    SSH_FX_EOF, SSH_FX_NO_SUCH_FILE, SSH_FX_PERMISSION_DENIED, SSH_FXF_APPEND, SSH_FXF_CREAT,
    SSH_FXF_EXCL, SSH_FXF_READ, SSH_FXF_TRUNC, SSH_FXF_WRITE, SftpFileAttrs, is_fatal_error,
    is_retryable_error, status_code_description,
};
use super::session::SftpSession;

/// Default buffer size for read operations (32 KB)
#[allow(dead_code)]
const DEFAULT_READ_BUF_SIZE: usize = 32 * 1024;
/// Default buffer size for write operations (32 KB)
#[allow(dead_code)]
const DEFAULT_WRITE_BUF_SIZE: usize = 32 * 1024;

// =============================================================================
// File Attributes
// =============================================================================

/// Represents file metadata as returned by SFTP STAT/LSTAT/FSTAT operations.
///
/// This struct mirrors the SFTP protocol v3 `attrs` structure with concrete
/// values (no Option wrappers) for ease of use in application code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAttributes {
    /// File size in bytes
    pub size: u64,
    /// Owner user ID
    pub uid: u32,
    /// Owner group ID
    pub gid: u32,
    /// Permission/mode bits (e.g., 0o644, 0o755)
    pub permissions: u32,
    /// Last access time (Unix timestamp)
    pub atime: u64,
    /// Last modification time (Unix timestamp)
    pub mtime: u64,
    /// True if this entry represents a directory
    pub is_directory: bool,
    /// True if this entry represents a regular file
    pub is_regular_file: bool,
    /// True if this entry represents a symbolic link
    pub is_symlink: bool,
}

impl Default for FileAttributes {
    fn default() -> Self {
        Self {
            size: 0,
            uid: 0,
            gid: 0,
            permissions: 0o644,
            atime: 0,
            mtime: 0,
            is_directory: false,
            is_regular_file: false,
            is_symlink: false,
        }
    }
}

impl std::fmt::Display for FileAttributes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let type_str = if self.is_directory {
            "dir"
        } else if self.is_regular_file {
            "file"
        } else if self.is_symlink {
            "symlink"
        } else {
            "unknown"
        };
        write!(
            f,
            "{}(size={}, perm={:o}, uid={}, gid={})",
            type_str, self.size, self.permissions, self.uid, self.gid
        )
    }
}

impl From<&SftpFileAttrs> for FileAttributes {
    /// Convert from packet-level SftpFileAttrs to the public FileAttributes.
    fn from(attrs: &SftpFileAttrs) -> Self {
        let perm = attrs.permissions.unwrap_or(0);
        Self {
            size: attrs.size.unwrap_or(0),
            uid: attrs.uid.unwrap_or(0),
            gid: attrs.gid.unwrap_or(0),
            permissions: perm,
            atime: attrs.atime.unwrap_or(0),
            mtime: attrs.mtime.unwrap_or(0),
            is_directory: (perm & 0o170000) == 0o040000,
            is_regular_file: (perm & 0o170000) == 0o100000,
            is_symlink: (perm & 0o170000) == 0o120000,
        }
    }
}

impl FileAttributes {
    /// Create attributes suitable for a regular file of given size.
    pub fn for_file(size: u64) -> Self {
        Self {
            size,
            is_regular_file: true,
            ..Default::default()
        }
    }

    /// Create attributes suitable for a directory.
    pub fn for_directory() -> Self {
        Self {
            permissions: 0o755,
            is_directory: true,
            ..Default::default()
        }
    }

    /// Check if this appears to be an empty file or directory.
    pub fn is_empty(&self) -> bool {
        self.size == 0 && !self.is_directory
    }

    /// Get the human-readable permission string (e.g., "rw-r--r--").
    pub fn permission_string(&self) -> String {
        format!("{:o}", self.permissions)
    }

    /// Create a SftpFileAttrs from this FileAttributes (for use in SETSTAT/FSETSTAT).
    pub fn to_sftp_attrs(&self) -> SftpFileAttrs {
        SftpFileAttrs {
            size: Some(self.size),
            uid: Some(self.uid),
            gid: Some(self.gid),
            permissions: Some(self.permissions),
            atime: Some(self.atime),
            mtime: Some(self.mtime),
            extended: Vec::new(),
        }
    }
}

// =============================================================================
// Open Flags
// =============================================================================

bitflags::bitflags! {
    /// Flags controlling how files are opened in SFTP operations.
    ///
    /// These map directly to the SSH_FXF_* constants defined in the SFTP protocol:
    /// - READ (0x00000001): Open for reading
    /// - WRITE (0x00000002): Open for writing
    /// - APPEND (0x00000004): Force writes to append
    /// - CREATE (0x00000008): Create file if it doesn't exist
    /// - TRUNCATE (0x00000010): Truncate existing file to zero length
    /// - CREATE_NEW (0x00000020): Exclusive create (fail if exists)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OpenFlags: u32 {
        const READ   = 0x0001;
        const WRITE  = 0x0002;
        const APPEND = 0x0004;
        const CREATE = 0x0008;
        const TRUNCATE = 0x0010;
        const CREATE_NEW = 0x0020;
    }
}

impl Default for OpenFlags {
    fn default() -> Self {
        Self::READ
    }
}

impl OpenFlags {
    /// Create flags for reading an existing file.
    pub fn readonly() -> Self {
        Self::READ
    }

    /// Create flags for writing (create or truncate).
    pub fn write_create() -> Self {
        Self::WRITE | Self::CREATE | Self::TRUNCATE
    }

    /// Create flags for appending to a file.
    pub fn append() -> Self {
        Self::WRITE | Self::APPEND | Self::CREATE
    }

    /// Create flags for resume writing (read+write without truncation).
    pub fn resume() -> Self {
        Self::READ | Self::WRITE | Self::CREATE
    }

    /// Convert our OpenFlags to the protocol-level SSH_FXF_* bitmask.
    pub fn to_protocol_flags(&self) -> u32 {
        let mut flags = 0u32;
        if self.contains(Self::READ) {
            flags |= SSH_FXF_READ;
        }
        if self.contains(Self::WRITE) || self.contains(Self::APPEND) {
            flags |= SSH_FXF_WRITE;
            // Many SFTP servers require READ access for WRITE operations
            // (e.g., to verify file existence before writing)
            flags |= SSH_FXF_READ;
        }
        if self.contains(Self::APPEND) {
            flags |= SSH_FXF_APPEND;
        }
        if self.contains(Self::CREATE) || self.contains(Self::CREATE_NEW) {
            flags |= SSH_FXF_CREAT;
        }
        if self.contains(Self::TRUNCATE) {
            flags |= SSH_FXF_TRUNC;
        }
        if self.contains(Self::CREATE_NEW) {
            flags |= SSH_FXF_EXCL;
        }
        flags
    }
}

// =============================================================================
// Directory Entry
// =============================================================================

/// A single entry returned by directory listing (READDIR) operations.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// The base filename (not the full path)
    pub name: String,
    /// Full file attributes for this entry
    pub attributes: FileAttributes,
}

impl std::fmt::Display for DirEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.attributes)
    }
}

// =============================================================================
// Error Types
// =============================================================================

/// Application-level error type for SFTP file operations.
///
/// Maps low-level SFTP status codes into categories that the download engine
/// can use for retry logic, user messaging, and logging decisions.
#[derive(Debug, Clone, thiserror::Error)]
pub enum FileOpError {
    /// The requested file does not exist on the remote server
    #[error("No such file or directory: {path}")]
    NotFound { path: String },

    /// Permission denied accessing the resource
    #[error("Permission denied: {path}")]
    PermissionDenied { path: String },

    /// A network or connection-level failure occurred
    #[error("Network/IO error on {operation}: {message}")]
    Network { operation: String, message: String },

    /// The operation is not supported by the server
    #[error("Operation not supported: {operation}")]
    Unsupported { operation: String },

    /// A generic SFTP protocol error occurred
    #[error("SFTP error ({code}) on {operation}: {message}")]
    Protocol {
        code: u32,
        operation: String,
        message: String,
    },

    /// Invalid input parameters
    #[error("Invalid argument: {message}")]
    InvalidArgument { message: String },
}

impl FileOpError {
    /// Map an SFTP status code + context to a FileOpError.
    ///
    /// Classifies the error based on the SFTP status code and operation context.
    pub fn from_status_code(code: u32, operation: &str, path: &str) -> Self {
        match code {
            SSH_FX_NO_SUCH_FILE => Self::NotFound {
                path: path.to_string(),
            },
            SSH_FX_PERMISSION_DENIED => Self::PermissionDenied {
                path: path.to_string(),
            },
            _ if is_fatal_error(code) => Self::Protocol {
                code,
                operation: operation.to_string(),
                message: status_code_description(code).to_string(),
            },
            _ if is_retryable_error(code) => Self::Network {
                operation: operation.to_string(),
                message: format!(
                    "retryable error {} ({})",
                    code,
                    status_code_description(code)
                ),
            },
            _ => Self::Protocol {
                code,
                operation: operation.to_string(),
                message: status_code_description(code).to_string(),
            },
        }
    }

    /// Check if this error indicates the target simply doesn't exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }

    /// Check if this error indicates a permission problem.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::PermissionDenied { .. })
    }

    /// Check if this error might be resolved by retrying.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Network { .. })
    }

    /// Get the path associated with this error, if any.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::NotFound { path } | Self::PermissionDenied { path } => Some(path),
            _ => None,
        }
    }
}

// =============================================================================
// SFTP File Handle
// =============================================================================

/// A handle to an open SFTP file on the remote server.
///
/// This wraps a server-side opaque handle (returned by SSH_FXP_OPEN)
/// and provides methods for positioned I/O operations through the SFTP session.
pub struct SftpFileHandle {
    /// The server-side opaque handle bytes (returned by OPEN response)
    handle_bytes: Vec<u8>,
    /// Reference to the owning session for issuing requests
    session: Arc<SftpSession>,
}

impl SftpFileHandle {
    /// Get file attributes via the open handle (FSTAT).
    ///
    /// Uses `SSH_FXP_FSTAT` which operates on the handle rather than path.
    pub async fn fstat(&self) -> Result<FileAttributes, FileOpError> {
        debug!("[SFTP] FSTAT on handle");

        let pkt = super::packet::SftpPacket::Fstat {
            request_id: 0, // Will be set by request()
            handle: self.handle_bytes.clone(),
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "FSTAT".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Attrs { attrs, .. } => Ok(FileAttributes::from(&attrs)),
            super::packet::SftpPacket::Status {
                code, message: _, ..
            } => Err(FileOpError::from_status_code(code, "FSTAT", "<handle>")),
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "FSTAT".to_string(),
                message: format!("unexpected packet type: {}", other.packet_type()),
            }),
        }
    }

    /// Set file attributes via the open handle (FSETSTAT).
    pub async fn set_fstat(&self, attrs: &FileAttributes) -> Result<(), FileOpError> {
        debug!("[SFTP] FSETSTAT on handle");

        let pkt = super::packet::SftpPacket::Fsetstat {
            request_id: 0,
            handle: self.handle_bytes.clone(),
            attrs: attrs.to_sftp_attrs(),
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "FSETSTAT".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Status { code: 0, .. } => Ok(()),
            super::packet::SftpPacket::Status {
                code, message: _, ..
            } => Err(FileOpError::from_status_code(code, "FSETSTAT", "<handle>")),
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "FSETSTAT".to_string(),
                message: format!("unexpected packet type: {}", other.packet_type()),
            }),
        }
    }

    /// Read data at a specific offset into the returned buffer.
    ///
    /// # Arguments
    /// * `offset` - Byte offset from start of file
    /// * `length` - Maximum number of bytes to read
    ///
    /// # Returns
    /// The data bytes read (may be less than `length` near EOF)
    pub async fn read_at(&self, offset: u64, length: u32) -> Result<Vec<u8>, FileOpError> {
        debug!("[SFTP] READ handle offset={} len={}", offset, length);

        let pkt = super::packet::SftpPacket::Read {
            request_id: 0,
            handle: self.handle_bytes.clone(),
            offset,
            length,
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "READ".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Data { data, .. } => Ok(data),
            super::packet::SftpPacket::Status { code, .. } if code == SSH_FX_EOF => {
                // EOF means no more data -- return empty buffer
                Ok(Vec::new())
            }
            super::packet::SftpPacket::Status {
                code, message: _, ..
            } => Err(FileOpError::from_status_code(code, "READ", "<handle>")),
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "READ".to_string(),
                message: format!("unexpected packet type: {}", other.packet_type()),
            }),
        }
    }

    /// Write data at a specific offset.
    ///
    /// # Arguments
    /// * `offset` - Byte offset from start of file
    /// * `data` - Data bytes to write
    ///
    /// # Returns
    /// Number of bytes written
    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<u64, FileOpError> {
        debug!("[SFTP] WRITE handle offset={} len={}", offset, data.len());

        let pkt = super::packet::SftpPacket::Write {
            request_id: 0,
            handle: self.handle_bytes.clone(),
            offset,
            data: data.to_vec(),
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "WRITE".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Status { code: 0, .. } => Ok(data.len() as u64),
            super::packet::SftpPacket::Status {
                code, message: _, ..
            } => Err(FileOpError::from_status_code(code, "WRITE", "<handle>")),
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "WRITE".to_string(),
                message: format!("unexpected packet type: {}", other.packet_type()),
            }),
        }
    }

    /// Close the file handle and release server-side resources.
    pub async fn close(self) -> Result<(), FileOpError> {
        debug!("[SFTP] Closing file handle");

        let pkt = super::packet::SftpPacket::Close {
            request_id: 0,
            handle: self.handle_bytes.clone(),
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "CLOSE".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Status { code: 0, .. } => {
                debug!("[SFTP] File handle closed successfully");
                Ok(())
            }
            super::packet::SftpPacket::Status { code, message, .. } => {
                warn!("[SFTP] Close returned error {}: {}", code, message);
                // Still return Ok since we're cleaning up
                Ok(())
            }
            other => {
                warn!(
                    "[SFTP] Close unexpected response: {:?}",
                    other.packet_type()
                );
                Ok(())
            }
        }
    }
}

// =============================================================================
// SFTP File Operations Interface
// =============================================================================

/// High-level interface for performing SFTP file and directory operations.
///
/// This struct wraps the underlying SFTP session and provides idiomatic
/// Rust methods for common SFTP operations with proper error handling.
/// Every operation goes through `session.request()` which handles
/// packet encoding, channel I/O, and response decoding.
///
/// # Usage
///
/// ```ignore
/// let session = SftpSession::open(&conn).await?;
/// let ops = SftpFileOps::new(&session);
///
/// // Stat a remote file
/// let attrs = ops.stat("/remote/file.txt")?;
///
/// // Read a file in chunks
/// let mut handle = ops.open("/remote/file.txt", OpenFlags::readonly(), 0)?;
/// let data = handle.read_at(0, 32768).await?;
/// ```
pub struct SftpFileOps<'a> {
    /// Reference to the owning SFTP session
    session: &'a SftpSession,
}

impl<'a> SftpFileOps<'a> {
    /// Create a new file operations handle bound to the given session.
    pub fn new(session: &'a SftpSession) -> Self {
        Self { session }
    }

    /// Get a reference to the underlying SFTP session.
    pub fn session(&self) -> &'a SftpSession {
        self.session
    }

    /// Helper: send a request and check for STATUS response (OK or error).
    async fn check_status(
        &self,
        pkt: super::packet::SftpPacket,
        operation: &str,
        path: &str,
    ) -> Result<(), FileOpError> {
        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: operation.to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Status { code: 0, .. } => Ok(()),
            super::packet::SftpPacket::Status { code, .. } => {
                Err(FileOpError::from_status_code(code, operation, path))
            }
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: operation.to_string(),
                message: format!("expected STATUS, got type={}", other.packet_type()),
            }),
        }
    }

    // -----------------------------------------------------------------
    // Attribute Operations (STAT/LSTAT/FSTAT)
    // -----------------------------------------------------------------

    /// Get file attributes for a path (follows symbolic links).
    ///
    /// Equivalent to the SFTP `SSH_FXP_STAT` request.
    pub async fn stat(&self, path: &str) -> Result<FileAttributes, FileOpError> {
        debug!("[SFTP] STAT: {}", path);

        let pkt = super::packet::SftpPacket::Stat {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "STAT".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Attrs { attrs, .. } => Ok(FileAttributes::from(&attrs)),
            super::packet::SftpPacket::Status { code, .. } => {
                Err(FileOpError::from_status_code(code, "STAT", path))
            }
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "STAT".to_string(),
                message: format!("expected ATTRS, got type={}", other.packet_type()),
            }),
        }
    }

    /// Get file attributes for a path (does NOT follow symbolic links).
    ///
    /// Equivalent to the SFTP `SSH_FXP_LSTAT` request.
    pub async fn lstat(&self, path: &str) -> Result<FileAttributes, FileOpError> {
        debug!("[SFTP] LSTAT: {}", path);

        let pkt = super::packet::SftpPacket::Lstat {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "LSTAT".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Attrs { attrs, .. } => Ok(FileAttributes::from(&attrs)),
            super::packet::SftpPacket::Status { code, .. } => {
                Err(FileOpError::from_status_code(code, "LSTAT", path))
            }
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "LSTAT".to_string(),
                message: format!("expected ATTRS, got type={}", other.packet_type()),
            }),
        }
    }

    /// Set file attributes by path.
    ///
    /// Equivalent to the SFTP `SSH_FXP_SETSTAT` request.
    pub async fn set_stat(&self, path: &str, attrs: &FileAttributes) -> Result<(), FileOpError> {
        debug!("[SFTP] SETSTAT: {} (perm={:o})", path, attrs.permissions);

        let pkt = super::packet::SftpPacket::Setstat {
            request_id: 0,
            path: path.to_string(),
            attrs: attrs.to_sftp_attrs(),
        };

        self.check_status(pkt, "SETSTAT", path).await
    }

    /// Resolve a path to its canonical form.
    ///
    /// Equivalent to the SFTP `SSH_FXP_REALPATH` request.
    /// Useful for resolving relative paths, `..`, symlinks, etc.
    pub async fn realpath(&self, path: &str) -> Result<String, FileOpError> {
        debug!("[SFTP] REALPATH: {}", path);

        let pkt = super::packet::SftpPacket::Realpath {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "REALPATH".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Name { entries, .. } => {
                // REALPATH returns a NAME packet with one entry; take the first filename
                if let Some(entry) = entries.into_iter().next() {
                    Ok(entry.filename)
                } else {
                    Err(FileOpError::Protocol {
                        code: 0,
                        operation: "REALPATH".to_string(),
                        message: "empty NAME response".to_string(),
                    })
                }
            }
            super::packet::SftpPacket::Status { code, .. } => {
                Err(FileOpError::from_status_code(code, "REALPATH", path))
            }
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "REALPATH".to_string(),
                message: format!("expected NAME, got type={}", other.packet_type()),
            }),
        }
    }

    // -----------------------------------------------------------------
    // File Open/Close Operations
    // -----------------------------------------------------------------

    /// Open a remote file with the specified flags.
    ///
    /// # Arguments
    /// * `path` - Remote file path to open
    /// * `flags` - Combination of OpenFlags specifying access mode
    /// * `mode` - Creation mode (permissions) if creating a new file (e.g., 0o644)
    ///
    /// # Returns
    /// An `SftpFileHandle` that can be used for read/write operations.
    pub async fn open(
        &self,
        path: &str,
        flags: OpenFlags,
        mode: u32,
    ) -> Result<SftpFileHandle, FileOpError> {
        debug!("[SFTP] OPEN: {} (mode={:o}, flags={:?})", path, mode, flags);

        let pkt = super::packet::SftpPacket::Open {
            request_id: 0,
            filename: path.to_string(),
            flags: flags.to_protocol_flags(),
            attrs: SftpFileAttrs {
                permissions: Some(mode),
                ..Default::default()
            },
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "OPEN".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Handle { handle, .. } => Ok(SftpFileHandle {
                handle_bytes: handle,
                session: Arc::new(self.session.clone()),
            }),
            super::packet::SftpPacket::Status { code, .. } => {
                Err(FileOpError::from_status_code(code, "OPEN", path))
            }
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "OPEN".to_string(),
                message: format!("expected HANDLE, got type={}", other.packet_type()),
            }),
        }
    }

    // -----------------------------------------------------------------
    // Directory Operations
    // -----------------------------------------------------------------

    /// Create a directory with the specified permissions.
    pub async fn mkdir(&self, path: &str, mode: u32) -> Result<(), FileOpError> {
        debug!("[SFTP] MKDIR: {} (mode={:o})", path, mode);

        let pkt = super::packet::SftpPacket::Mkdir {
            request_id: 0,
            path: path.to_string(),
            attrs: SftpFileAttrs {
                permissions: Some(mode),
                ..Default::default()
            },
        };

        self.check_status(pkt, "MKDIR", path).await
    }

    /// Recursively create directories (like `mkdir -p`).
    pub async fn mkdir_recursive(&self, path: &str, mode: u32) -> Result<(), FileOpError> {
        // Parse path components
        let mut current = String::new();
        if let Some('/') = path.chars().next() {
            current.push('/');
        }

        for component in path.split('/').filter(|s| !s.is_empty()) {
            current.push_str(component);
            // Try to create; ignore "already exists" errors
            match self.mkdir(&current, mode).await {
                Ok(_) | Err(FileOpError::NotFound { .. }) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
            current.push('/');
        }

        // Final mkdir without trailing slash for exact path
        if path.ends_with('/') {
            Ok(())
        } else {
            self.mkdir(path, mode)
                .await
                .or_else(|e| if e.is_not_found() { Ok(()) } else { Err(e) })
        }
    }

    /// Remove an empty directory.
    pub async fn rmdir(&self, path: &str) -> Result<(), FileOpError> {
        debug!("[SFTP] RMDIR: {}", path);

        let pkt = super::packet::SftpPacket::Rmdir {
            request_id: 0,
            path: path.to_string(),
        };

        self.check_status(pkt, "RMDIR", path).await
    }

    /// List contents of a directory.
    ///
    /// Returns entries excluding `.` and `..`.
    pub async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, FileOpError> {
        debug!("[SFTP] READDIR: {}", path);

        // Step 1: OPENDIR
        let opendir_pkt = super::packet::SftpPacket::Opendir {
            request_id: 0,
            path: path.to_string(),
        };
        let opendir_resp =
            self.session
                .request(opendir_pkt)
                .await
                .map_err(|e| FileOpError::Network {
                    operation: "OPENDIR".to_string(),
                    message: e,
                })?;

        let dir_handle = match opendir_resp {
            super::packet::SftpPacket::Handle { handle, .. } => handle,
            super::packet::SftpPacket::Status { code, .. } => {
                return Err(FileOpError::from_status_code(code, "OPENDIR", path));
            }
            other => {
                return Err(FileOpError::Protocol {
                    code: other.packet_type() as u32,
                    operation: "OPENDIR".to_string(),
                    message: format!("expected HANDLE, got type={}", other.packet_type()),
                });
            }
        };

        // Step 2: Loop READDIR until EOF or empty NAME
        let mut entries = Vec::new();
        loop {
            let readdir_pkt = super::packet::SftpPacket::Readdir {
                request_id: 0,
                handle: dir_handle.clone(),
            };

            let resp =
                self.session
                    .request(readdir_pkt)
                    .await
                    .map_err(|e| FileOpError::Network {
                        operation: "READDIR".to_string(),
                        message: e,
                    })?;

            match resp {
                super::packet::SftpPacket::Name {
                    entries: mut name_entries,
                    ..
                } => {
                    if name_entries.is_empty() {
                        break; // End of directory listing
                    }
                    for entry in name_entries.drain(..) {
                        let name = entry.filename;
                        // Skip special directory entries
                        if name == "." || name == ".." {
                            continue;
                        }
                        entries.push(DirEntry {
                            name,
                            attributes: FileAttributes::from(&entry.attrs),
                        });
                    }
                }
                super::packet::SftpPacket::Status { code, .. } if code == SSH_FX_EOF => {
                    break; // Normal end of directory listing
                }
                super::packet::SftpPacket::Status { code, message, .. } => {
                    warn!("[SFTP] READDIR error [{}]: {} ({})", path, code, message);
                    break;
                }
                other => {
                    warn!("[SFTP] READDIR unexpected: type={}", other.packet_type());
                    break;
                }
            }
        }

        // Step 3: CLOSE the dir handle (best-effort)
        let close_pkt = super::packet::SftpPacket::Close {
            request_id: 0,
            handle: dir_handle,
        };
        let _ = self.session.request(close_pkt).await;

        debug!(
            "[SFTP] READDIR: {} returned {} entries",
            path,
            entries.len()
        );
        Ok(entries)
    }

    /// List only regular files (and symlinks) in a directory.
    pub async fn list_files(&self, path: &str) -> Result<Vec<String>, FileOpError> {
        let entries = self.readdir(path).await?;
        Ok(entries
            .into_iter()
            .filter(|e| e.attributes.is_regular_file || e.attributes.is_symlink)
            .map(|e| e.name)
            .collect())
    }

    /// List only subdirectories in a directory (excluding . and ..).
    pub async fn list_dirs(&self, path: &str) -> Result<Vec<String>, FileOpError> {
        let entries = self.readdir(path).await?;
        Ok(entries
            .into_iter()
            .filter(|e| e.attributes.is_directory)
            .map(|e| e.name)
            .collect())
    }

    // -----------------------------------------------------------------
    // File System Operations
    // -----------------------------------------------------------------

    /// Remove (unlink) a file.
    pub async fn unlink(&self, path: &str) -> Result<(), FileOpError> {
        debug!("[SFTP] UNLINK: {}", path);

        let pkt = super::packet::SftpPacket::Remove {
            request_id: 0,
            filename: path.to_string(),
        };

        self.check_status(pkt, "UNLINK", path).await
    }

    /// Rename a file or directory.
    pub async fn rename(&self, src: &str, dest: &str) -> Result<(), FileOpError> {
        debug!("[SFTP] RENAME: {} -> {}", src, dest);

        let pkt = super::packet::SftpPacket::Rename {
            request_id: 0,
            old_path: src.to_string(),
            new_path: dest.to_string(),
        };

        self.check_status(pkt, "RENAME", src).await
    }

    /// Create a symbolic link.
    pub async fn symlink(&self, target: &str, link_path: &str) -> Result<(), FileOpError> {
        debug!("[SFTP] SYMLINK: {} -> {}", link_path, target);

        let pkt = super::packet::SftpPacket::Symlink {
            request_id: 0,
            link_path: link_path.to_string(),
            target_path: target.to_string(),
        };

        self.check_status(pkt, "SYMLINK", link_path).await
    }

    /// Read the target of a symbolic link.
    pub async fn readlink(&self, path: &str) -> Result<String, FileOpError> {
        debug!("[SFTP] READLINK: {}", path);

        let pkt = super::packet::SftpPacket::Readlink {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = self
            .session
            .request(pkt)
            .await
            .map_err(|e| FileOpError::Network {
                operation: "READLINK".to_string(),
                message: e,
            })?;

        match resp {
            super::packet::SftpPacket::Name { mut entries, .. } => {
                if let Some(entry) = entries.pop() {
                    Ok(entry.filename)
                } else {
                    Err(FileOpError::Protocol {
                        code: 0,
                        operation: "READLINK".to_string(),
                        message: "empty NAME response".to_string(),
                    })
                }
            }
            super::packet::SftpPacket::Status { code, .. } => {
                Err(FileOpError::from_status_code(code, "READLINK", path))
            }
            other => Err(FileOpError::Protocol {
                code: other.packet_type() as u32,
                operation: "READLINK".to_string(),
                message: format!("expected NAME, got type={}", other.packet_type()),
            }),
        }
    }

    /// Check whether a path exists (without following symlinks for final component).
    pub async fn exists(&self, path: &str) -> Result<bool, FileOpError> {
        match self.lstat(path).await {
            Ok(_) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Check if a path is a regular file.
    pub async fn is_file(&self, path: &str) -> Result<bool, FileOpError> {
        let attr = self.stat(path).await?;
        Ok(attr.is_regular_file)
    }

    /// Check if a path is a directory.
    pub async fn is_dir(&self, path: &str) -> Result<bool, FileOpError> {
        let attr = self.stat(path).await?;
        Ok(attr.is_directory)
    }
}

// =============================================================================
// Note: Clone for SftpSession is implemented in session.rs
// (it requires access to private fields)
// =============================================================================

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_attributes_default() {
        let attrs = FileAttributes::default();
        assert_eq!(attrs.size, 0);
        assert_eq!(attrs.permissions, 0o644);
        assert!(!attrs.is_directory);
        assert!(!attrs.is_regular_file);
        assert!(!attrs.is_symlink);
    }

    #[test]
    fn test_file_attributes_for_file() {
        let attrs = FileAttributes::for_file(99999);
        assert_eq!(attrs.size, 99999);
        assert!(attrs.is_regular_file);
        assert!(!attrs.is_directory);
    }

    #[test]
    fn test_file_attributes_for_directory() {
        let attrs = FileAttributes::for_directory();
        assert!(attrs.is_directory);
        assert_eq!(attrs.permissions, 0o755);
        assert!(!attrs.is_regular_file);
    }

    #[test]
    fn test_file_attributes_display() {
        let attrs = FileAttributes {
            size: 12345,
            permissions: 0o644,
            uid: 1000,
            gid: 1000,
            is_regular_file: true,
            ..Default::default()
        };
        let display = format!("{}", attrs);
        assert!(display.contains("file"));
        assert!(display.contains("12345"));
    }

    #[test]
    fn test_permission_string() {
        let attrs = FileAttributes {
            permissions: 0o755,
            ..Default::default()
        };
        assert_eq!(attrs.permission_string(), "755");
    }

    #[test]
    fn test_file_attrs_conversion_roundtrip() {
        // Test conversion from SftpFileAttrs to FileAttributes and back
        let sftp_attrs = SftpFileAttrs::full(12345, 1000, 1000, 0o100644, 1704000000, 1704000100);
        let file_attrs = FileAttributes::from(&sftp_attrs);
        assert_eq!(file_attrs.size, 12345);
        assert_eq!(file_attrs.uid, 1000);
        assert_eq!(file_attrs.permissions, 0o100644);
        assert!(file_attrs.is_regular_file);

        // Convert back
        let roundtrip = file_attrs.to_sftp_attrs();
        assert_eq!(roundtrip.size, Some(12345));
        assert_eq!(roundtrip.uid, Some(1000));
    }

    #[test]
    fn test_open_flags_variants() {
        assert_eq!(OpenFlags::readonly(), OpenFlags::READ);
        let wc = OpenFlags::write_create();
        assert!(wc.contains(OpenFlags::WRITE));
        assert!(wc.contains(OpenFlags::CREATE));
        assert!(wc.contains(OpenFlags::TRUNCATE));

        let app = OpenFlags::append();
        assert!(app.contains(OpenFlags::APPEND));

        let res = OpenFlags::resume();
        assert!(res.contains(OpenFlags::READ));
        assert!(res.contains(OpenFlags::WRITE));
        assert!(!res.contains(OpenFlags::TRUNCATE));
    }

    #[test]
    fn test_open_flags_to_protocol_flags() {
        let readonly = OpenFlags::readonly();
        assert_eq!(readonly.to_protocol_flags(), SSH_FXF_READ);

        let write_create = OpenFlags::write_create();
        let pf = write_create.to_protocol_flags();
        assert!(pf & SSH_FXF_READ != 0); // WRITE implies READ in many servers
        assert!(pf & SSH_FXF_WRITE != 0);
        assert!(pf & SSH_FXF_CREAT != 0);
        assert!(pf & SSH_FXF_TRUNC != 0);
    }

    #[test]
    fn test_open_flags_combinations() {
        let combined = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
        assert!(combined.contains(OpenFlags::READ));
        assert!(combined.contains(OpenFlags::WRITE));
        assert!(combined.contains(OpenFlags::CREATE));
        assert!(!combined.contains(OpenFlags::TRUNCATE));
        assert!(!combined.contains(OpenFlags::APPEND));
    }

    #[test]
    fn test_open_flags_default_is_readonly() {
        assert_eq!(OpenFlags::default(), OpenFlags::READ);
    }

    #[test]
    fn test_dir_entry_display() {
        let entry = DirEntry {
            name: "test.txt".to_string(),
            attributes: FileAttributes::for_file(2048),
        };
        let display = format!("{}", entry);
        assert!(display.contains("test.txt"));
        assert!(display.contains("2048"));
    }

    #[test]
    fn test_buf_size_constants() {
        assert_eq!(DEFAULT_READ_BUF_SIZE, 32768); // 32KB
        assert_eq!(DEFAULT_WRITE_BUF_SIZE, 32768); // 32KB
    }

    // -----------------------------------------------------------------
    // FileOpError Classification Tests
    // -----------------------------------------------------------------

    #[test]
    fn test_file_op_error_not_found_detection() {
        let err = FileOpError::NotFound {
            path: "/missing.txt".to_string(),
        };
        assert!(err.is_not_found());
        assert!(!err.is_permission_denied());
        assert!(!err.is_retryable());
        assert_eq!(err.path(), Some("/missing.txt"));
    }

    #[test]
    fn test_file_op_error_permission_denied_detection() {
        let err = FileOpError::PermissionDenied {
            path: "/secret".to_string(),
        };
        assert!(err.is_permission_denied());
        assert!(!err.is_not_found());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_file_op_error_network_is_retryable() {
        let err = FileOpError::Network {
            operation: "READ".to_string(),
            message: "Connection reset".to_string(),
        };
        assert!(err.is_retryable());
        assert!(!err.is_not_found());
        assert!(err.path().is_none());
    }

    #[test]
    fn test_file_op_error_protocol_not_retryable() {
        let err = FileOpError::Protocol {
            code: 4, // SSH_FX_FAILURE
            operation: "OPEN".to_string(),
            message: "Generic failure".to_string(),
        };
        // Generic FAILURE is not classified as retryable by default
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_file_op_error_display_messages() {
        let err = FileOpError::NotFound {
            path: "/data/file.bin".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("No such file"));
        assert!(msg.contains("/data/file.bin"));

        let err = FileOpError::PermissionDenied {
            path: "/root/.ssh".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("Permission denied"));
    }

    #[test]
    fn test_file_op_error_invalid_argument() {
        let err = FileOpError::InvalidArgument {
            message: "empty path".to_string(),
        };
        assert!(format!("{}", err).contains("empty path"));
    }

    #[test]
    fn test_from_status_code_mapping() {
        // No such file
        let err = FileOpError::from_status_code(SSH_FX_NO_SUCH_FILE, "STAT", "/missing");
        assert!(err.is_not_found());

        // Permission denied
        let err = FileOpError::from_status_code(SSH_FX_PERMISSION_DENIED, "OPEN", "/secret");
        assert!(err.is_permission_denied());

        // EOF should map to network/retryable
        let err = FileOpError::from_status_code(SSH_FX_EOF, "READ", "/file");
        assert!(err.is_retryable());

        // OK (code=0) should not produce an error normally, but if used:
        let err = FileOpError::from_status_code(0, "CLOSE", "/file");
        match err {
            FileOpError::Protocol { .. } => {} // OK maps to Protocol with success description
            other => panic!("Unexpected error type for OK: {:?}", other),
        }
    }
}
