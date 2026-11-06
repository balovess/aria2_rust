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
//! delegate to the session's single serialized request path for request ID
//! assignment, response correlation, and packet I/O.

use tracing::{debug, warn};

use super::packet::{SSH_FX_EOF, SSH_FX_OK, SftpFileAttrs, SftpPacket};
use super::session::SftpSession;

#[cfg(test)]
mod tests;
mod types;

pub use types::{FileAttributes, FileOpError, OpenFlags};

async fn request(
    session: &SftpSession,
    packet: SftpPacket,
    operation: &'static str,
) -> Result<SftpPacket, FileOpError> {
    session
        .request(packet)
        .await
        .map_err(|error| FileOpError::session(operation, error))
}

fn unexpected_response(operation: &str, packet: SftpPacket) -> FileOpError {
    FileOpError::other(format!(
        "Unexpected response to {operation}: type={}",
        packet.packet_type()
    ))
}

fn expect_handle(
    response: SftpPacket,
    operation: &'static str,
    path: Option<&str>,
) -> Result<Vec<u8>, FileOpError> {
    match response {
        SftpPacket::Handle { handle, .. } => Ok(handle),
        SftpPacket::Status { code, message, .. } => {
            Err(FileOpError::from_status(operation, code, message, path))
        }
        other => Err(unexpected_response(operation, other)),
    }
}

fn expect_attrs(
    response: SftpPacket,
    operation: &'static str,
    path: &str,
) -> Result<FileAttributes, FileOpError> {
    match response {
        SftpPacket::Attrs { attrs, .. } => Ok(FileAttributes::from_wire(&attrs)),
        SftpPacket::Status { code, message, .. } => Err(FileOpError::from_status(
            operation,
            code,
            message,
            Some(path),
        )),
        other => Err(unexpected_response(operation, other)),
    }
}

fn expect_ok(
    response: SftpPacket,
    operation: &'static str,
    path: Option<&str>,
) -> Result<(), FileOpError> {
    match response {
        SftpPacket::Status {
            code: SSH_FX_OK, ..
        } => Ok(()),
        SftpPacket::Status { code, message, .. } => {
            Err(FileOpError::from_status(operation, code, message, path))
        }
        other => Err(unexpected_response(operation, other)),
    }
}

fn expect_name(
    response: SftpPacket,
    operation: &'static str,
    path: &str,
) -> Result<String, FileOpError> {
    match response {
        SftpPacket::Name { entries, .. } => entries
            .into_iter()
            .next()
            .map(|entry| entry.filename)
            .ok_or_else(|| FileOpError::other(format!("{operation} returned an empty name list"))),
        SftpPacket::Status { code, message, .. } => Err(FileOpError::from_status(
            operation,
            code,
            message,
            Some(path),
        )),
        other => Err(unexpected_response(operation, other)),
    }
}

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
    pub async fn read_at(&self, offset: u64, len: u32) -> Result<Vec<u8>, FileOpError> {
        let pkt = SftpPacket::Read {
            request_id: 0, // Will be set by session.request()
            handle: self.handle.clone(),
            offset,
            length: len,
        };

        let resp = request(self.session, pkt, "READ").await?;

        match resp {
            SftpPacket::Data { data, .. } => Ok(data),
            SftpPacket::Status { code, .. } if code == SSH_FX_EOF => Ok(Vec::new()),
            SftpPacket::Status { code, message, .. } => {
                Err(FileOpError::from_status("Read", code, message, None))
            }
            other => Err(unexpected_response("READ", other)),
        }
    }

    /// Write `data` starting at `offset` to the remote file.
    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), FileOpError> {
        let pkt = SftpPacket::Write {
            request_id: 0,
            handle: self.handle.clone(),
            offset,
            data: data.to_vec(),
        };

        let resp = request(self.session, pkt, "WRITE").await?;
        expect_ok(resp, "Write", None)
    }

    /// Close the remote file handle explicitly.
    ///
    /// Sends SSH_FXP_CLOSE and marks the handle as closed. Calling `close()`
    /// more than once is a no-op.
    pub async fn close(&mut self) -> Result<(), FileOpError> {
        if self.closed {
            return Ok(());
        }

        let pkt = SftpPacket::Close {
            request_id: 0,
            handle: self.handle.clone(),
        };

        let resp = request(self.session, pkt, "CLOSE").await?;
        expect_ok(resp, "Close", None)?;
        self.closed = true;
        Ok(())
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
    ) -> Result<SftpRemoteFile<'a>, FileOpError> {
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

        let resp = request(self.session, pkt, "OPEN").await?;
        let handle = expect_handle(resp, "Open", Some(path))?;
        debug!("[SFTP] open() got handle (len={})", handle.len());
        Ok(SftpRemoteFile::new(self.session, handle))
    }

    // -----------------------------------------------------------------
    // Stat / Lstat
    // -----------------------------------------------------------------

    /// Get file attributes, following symlinks (SSH_FXP_STAT).
    pub async fn stat(&self, path: &str) -> Result<FileAttributes, FileOpError> {
        let pkt = SftpPacket::Stat {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = request(self.session, pkt, "STAT").await?;
        expect_attrs(resp, "Stat", path)
    }

    /// Get file attributes without following symlinks (SSH_FXP_LSTAT).
    pub async fn lstat(&self, path: &str) -> Result<FileAttributes, FileOpError> {
        let pkt = SftpPacket::Lstat {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = request(self.session, pkt, "LSTAT").await?;
        expect_attrs(resp, "Lstat", path)
    }

    // -----------------------------------------------------------------
    // Setstat
    // -----------------------------------------------------------------

    /// Set file attributes by path (SSH_FXP_SETSTAT).
    pub async fn set_stat(&self, path: &str, attrs: &FileAttributes) -> Result<(), FileOpError> {
        let pkt = SftpPacket::Setstat {
            request_id: 0,
            path: path.to_string(),
            attrs: attrs.to_wire(),
        };

        let resp = request(self.session, pkt, "SETSTAT").await?;
        expect_ok(resp, "Setstat", Some(path))
    }

    // -----------------------------------------------------------------
    // Directory Operations
    // -----------------------------------------------------------------

    /// Open a directory for listing (SSH_FXP_OPENDIR).
    pub async fn opendir(&self, path: &str) -> Result<SftpRemoteFile<'a>, FileOpError> {
        let pkt = SftpPacket::Opendir {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = request(self.session, pkt, "OPENDIR").await?;
        let handle = expect_handle(resp, "Opendir", Some(path))?;
        Ok(SftpRemoteFile::new(self.session, handle))
    }

    /// Read directory entries from an open directory handle (SSH_FXP_READDIR).
    ///
    /// Returns a list of `(filename, longname, FileAttributes)` tuples.
    /// An empty list indicates end-of-directory.
    pub async fn readdir(
        &self,
        dir_handle: &SftpRemoteFile<'_>,
    ) -> Result<Vec<(String, String, FileAttributes)>, FileOpError> {
        let pkt = SftpPacket::Readdir {
            request_id: 0,
            handle: dir_handle.handle.clone(),
        };

        let resp = request(self.session, pkt, "READDIR").await?;

        match resp {
            SftpPacket::Name { entries, .. } => Ok(entries
                .into_iter()
                .map(|e| (e.filename, e.longname, FileAttributes::from_wire(&e.attrs)))
                .collect()),
            SftpPacket::Status { code, .. } if code == SSH_FX_EOF => Ok(Vec::new()),
            SftpPacket::Status { code, message, .. } => {
                Err(FileOpError::from_status("Readdir", code, message, None))
            }
            other => Err(unexpected_response("READDIR", other)),
        }
    }

    /// Create a directory (SSH_FXP_MKDIR).
    pub async fn mkdir(&self, path: &str) -> Result<(), FileOpError> {
        let pkt = SftpPacket::Mkdir {
            request_id: 0,
            path: path.to_string(),
            attrs: SftpFileAttrs::default(),
        };

        let resp = request(self.session, pkt, "MKDIR").await?;
        expect_ok(resp, "Mkdir", Some(path))
    }

    /// Remove a directory (SSH_FXP_RMDIR).
    pub async fn rmdir(&self, path: &str) -> Result<(), FileOpError> {
        let pkt = SftpPacket::Rmdir {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = request(self.session, pkt, "RMDIR").await?;
        expect_ok(resp, "Rmdir", Some(path))
    }

    // -----------------------------------------------------------------
    // File Manipulation
    // -----------------------------------------------------------------

    /// Delete a file (SSH_FXP_REMOVE).
    pub async fn remove(&self, path: &str) -> Result<(), FileOpError> {
        let pkt = SftpPacket::Remove {
            request_id: 0,
            filename: path.to_string(),
        };

        let resp = request(self.session, pkt, "REMOVE").await?;
        expect_ok(resp, "Remove", Some(path))
    }

    /// Rename a file or directory (SSH_FXP_RENAME).
    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<(), FileOpError> {
        let pkt = SftpPacket::Rename {
            request_id: 0,
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        };

        let resp = request(self.session, pkt, "RENAME").await?;
        expect_ok(resp, "Rename", Some(old_path))
    }

    /// Canonicalize a path (SSH_FXP_REALPATH).
    pub async fn realpath(&self, path: &str) -> Result<String, FileOpError> {
        let pkt = SftpPacket::Realpath {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = request(self.session, pkt, "REALPATH").await?;
        expect_name(resp, "REALPATH", path)
    }

    /// Read the target of a symbolic link (SSH_FXP_READLINK).
    pub async fn readlink(&self, path: &str) -> Result<String, FileOpError> {
        let pkt = SftpPacket::Readlink {
            request_id: 0,
            path: path.to_string(),
        };

        let resp = request(self.session, pkt, "READLINK").await?;
        expect_name(resp, "READLINK", path)
    }

    /// Create a symbolic link (SSH_FXP_SYMLINK).
    pub async fn symlink(&self, link_path: &str, target_path: &str) -> Result<(), FileOpError> {
        let pkt = SftpPacket::Symlink {
            request_id: 0,
            link_path: link_path.to_string(),
            target_path: target_path.to_string(),
        };

        let resp = request(self.session, pkt, "SYMLINK").await?;
        expect_ok(resp, "Symlink", Some(link_path))
    }
}

// =============================================================================
// Unit Tests
// =============================================================================
