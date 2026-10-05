//! Public SFTP packet data types.

use super::attrs::SftpFileAttrs;
// SftpPacket -- top-level protocol packet enum
// =============================================================================

/// Represents a single SFTP protocol packet (request, response, or control).
///
/// Each variant maps to an SSH_FXP_* message type. Request packets carry a
/// `request_id` field for correlation with their response.
#[derive(Debug, Clone, PartialEq)]
pub enum SftpPacket {
    // -- Control packets (no request_id) --
    /// SSH_FXP_INIT: client initiates session with desired version
    Init { version: u32 },
    /// SSH_FXP_VERSION: server responds with supported version + extensions
    Version {
        version: u32,
        extensions: Vec<(String, String)>,
    },

    // -- Request packets (carry request_id) --
    /// SSH_FXP_OPEN: open or create a file
    Open {
        request_id: u32,
        filename: String,
        flags: u32,
        attrs: SftpFileAttrs,
    },
    /// SSH_FXP_CLOSE: close an open file/directory handle
    Close { request_id: u32, handle: Vec<u8> },
    /// SSH_FXP_READ: read data from an open file handle
    Read {
        request_id: u32,
        handle: Vec<u8>,
        offset: u64,
        length: u32,
    },
    /// SSH_FXP_WRITE: write data to an open file handle
    Write {
        request_id: u32,
        handle: Vec<u8>,
        offset: u64,
        data: Vec<u8>,
    },
    /// SSH_FXP_LSTAT: stat a path without following symlinks
    Lstat { request_id: u32, path: String },
    /// SSH_FXP_FSTAT: stat an open file handle
    Fstat { request_id: u32, handle: Vec<u8> },
    /// SSH_FXP_SETSTAT: set file attributes by path
    Setstat {
        request_id: u32,
        path: String,
        attrs: SftpFileAttrs,
    },
    /// SSH_FXP_FSETSTAT: set file attributes by handle
    Fsetstat {
        request_id: u32,
        handle: Vec<u8>,
        attrs: SftpFileAttrs,
    },
    /// SSH_FXP_OPENDIR: open a directory for listing
    Opendir { request_id: u32, path: String },
    /// SSH_FXP_READDIR: read directory entries from a handle
    Readdir { request_id: u32, handle: Vec<u8> },
    /// SSH_FXP_REMOVE: delete a file
    Remove { request_id: u32, filename: String },
    /// SSH_FXP_MKDIR: create a directory
    Mkdir {
        request_id: u32,
        path: String,
        attrs: SftpFileAttrs,
    },
    /// SSH_FXP_RMDIR: remove a directory
    Rmdir { request_id: u32, path: String },
    /// SSH_FXP_REALPATH: canonicalize a path
    Realpath { request_id: u32, path: String },
    /// SSH_FXP_STAT: stat a path, following symlinks
    Stat { request_id: u32, path: String },
    /// SSH_FXP_RENAME: rename a file or directory
    Rename {
        request_id: u32,
        old_path: String,
        new_path: String,
    },
    /// SSH_FXP_READLINK: read the target of a symbolic link
    Readlink { request_id: u32, path: String },
    /// SSH_FXP_SYMLINK: create a symbolic link
    Symlink {
        request_id: u32,
        link_path: String,
        target_path: String,
    },

    // -- Response packets (carry request_id) --
    /// SSH_FXP_STATUS: status/error response
    Status {
        request_id: u32,
        code: u32,
        message: String,
        language: String,
    },
    /// SSH_FXP_HANDLE: file/directory handle response
    Handle { request_id: u32, handle: Vec<u8> },
    /// SSH_FXP_DATA: file data response
    Data { request_id: u32, data: Vec<u8> },
    /// SSH_FXP_NAME: directory listing response
    Name {
        request_id: u32,
        entries: Vec<SftpNameEntry>,
    },
    /// SSH_FXP_ATTRS: attribute-only response
    Attrs {
        request_id: u32,
        attrs: SftpFileAttrs,
    },
}

/// A single entry in an SSH_FXP_NAME directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpNameEntry {
    /// File name (not full path)
    pub filename: String,
    /// Long format listing (like `ls -l`), may be empty
    pub longname: String,
    /// File attributes
    pub attrs: SftpFileAttrs,
}
