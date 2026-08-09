//! SFTP packet types, wire encoding, and protocol constants.
//!
//! Implements the SSH File Transfer Protocol (IETF draft-ietf-secsh-filexfer-02 / v3-v6)
//! packet types with binary encode/decode for the russh channel transport.
//!
//! ## Wire Format
//!
//! ```text
//! +----------------+----------------+
//! | uint32 length  | payload bytes  |
//! +----------------+----------------+
//!
//! Payload layout depends on packet type:
//!   INIT:     type(1) + version(4)
//!   VERSION:  type(1) + version(4) + extensions...
//!   Request:  type(1) + request_id(4) + ...
//!   Response: type(1) + request_id(4) + ...
//! ```

use std::io::{self, Read, Write};

// =============================================================================
// Protocol Type Codes (SSH_FXP_*)
// =============================================================================

/// SSH_FXP_INIT -- client -> server, starts a session
pub const SSH_FXP_INIT: u8 = 1;
/// SSH_FXP_VERSION -- server -> client, version reply
pub const SSH_FXP_VERSION: u8 = 2;
/// SSH_FXP_OPEN -- open a file
pub const SSH_FXP_OPEN: u8 = 3;
/// SSH_FXP_CLOSE -- close a handle
pub const SSH_FXP_CLOSE: u8 = 4;
/// SSH_FXP_READ -- read from a handle
pub const SSH_FXP_READ: u8 = 5;
/// SSH_FXP_WRITE -- write to a handle
pub const SSH_FXP_WRITE: u8 = 6;
/// SSH_FXP_LSTAT -- stat without following symlinks
pub const SSH_FXP_LSTAT: u8 = 7;
/// SSH_FXP_FSTAT -- stat by handle
pub const SSH_FXP_FSTAT: u8 = 8;
/// SSH_FXP_SETSTAT -- set attributes by path
pub const SSH_FXP_SETSTAT: u8 = 9;
/// SSH_FXP_FSETSTAT -- set attributes by handle
pub const SSH_FXP_FSETSTAT: u8 = 10;
/// SSH_FXP_OPENDIR -- open a directory for listing
pub const SSH_FXP_OPENDIR: u8 = 11;
/// SSH_FXP_READDIR -- read directory entries
pub const SSH_FXP_READDIR: u8 = 12;
/// SSH_FXP_REMOVE -- remove a file
pub const SSH_FXP_REMOVE: u8 = 13;
/// SSH_FXP_MKDIR -- create a directory
pub const SSH_FXP_MKDIR: u8 = 14;
/// SSH_FXP_RMDIR -- remove a directory
pub const SSH_FXP_RMDIR: u8 = 15;
/// SSH_FXP_REALPATH -- canonicalize a path
pub const SSH_FXP_REALPATH: u8 = 16;
/// SSH_FXP_STAT -- stat following symlinks
pub const SSH_FXP_STAT: u8 = 17;
/// SSH_FXP_RENAME -- rename a file
pub const SSH_FXP_RENAME: u8 = 18;
/// SSH_FXP_READLINK -- read a symbolic link
pub const SSH_FXP_READLINK: u8 = 19;
/// SSH_FXP_SYMLINK -- create a symbolic link
pub const SSH_FXP_SYMLINK: u8 = 20;
/// SSH_FXP_STATUS -- status response
pub const SSH_FXP_STATUS: u8 = 101;
/// SSH_FXP_HANDLE -- handle response
pub const SSH_FXP_HANDLE: u8 = 102;
/// SSH_FXP_DATA -- data response
pub const SSH_FXP_DATA: u8 = 103;
/// SSH_FXP_NAME -- name response (directory listing)
pub const SSH_FXP_NAME: u8 = 104;
/// SSH_FXP_ATTRS -- attribute response
pub const SSH_FXP_ATTRS: u8 = 105;

// =============================================================================
// SFTP Status Codes (SSH_FX_*)
// =============================================================================

/// SSH_FX_OK -- operation succeeded
pub const SSH_FX_OK: u32 = 0;
/// SSH_FX_EOF -- end of file
pub const SSH_FX_EOF: u32 = 1;
/// SSH_FX_NO_SUCH_FILE -- file not found
pub const SSH_FX_NO_SUCH_FILE: u32 = 2;
/// SSH_FX_PERMISSION_DENIED -- access denied
pub const SSH_FX_PERMISSION_DENIED: u32 = 3;
/// SSH_FX_FAILURE -- generic failure
pub const SSH_FX_FAILURE: u32 = 4;
/// SSH_FX_BAD_MESSAGE -- malformed message
pub const SSH_FX_BAD_MESSAGE: u32 = 5;
/// SSH_FX_NO_CONNECTION -- no connection
pub const SSH_FX_NO_CONNECTION: u32 = 6;
/// SSH_FX_CONNECTION_LOST -- connection lost
pub const SSH_FX_CONNECTION_LOST: u32 = 7;
/// SSH_FX_OP_UNSUPPORTED -- unsupported operation
pub const SSH_FX_OP_UNSUPPORTED: u32 = 8;

/// Return a human-readable description for a standard SFTP status code.
pub fn status_code_description(code: u32) -> &'static str {
    match code {
        SSH_FX_OK => "Operation succeeded",
        SSH_FX_EOF => "End of file",
        SSH_FX_NO_SUCH_FILE => "No such file",
        SSH_FX_PERMISSION_DENIED => "Permission denied",
        SSH_FX_FAILURE => "Generic failure",
        SSH_FX_BAD_MESSAGE => "Bad message",
        SSH_FX_NO_CONNECTION => "No connection",
        SSH_FX_CONNECTION_LOST => "Connection lost",
        SSH_FX_OP_UNSUPPORTED => "Operation unsupported",
        _ => "Unknown status code",
    }
}

// =============================================================================
// SFTP Open Flags (SSH_FXF_*)
// =============================================================================

/// SSH_FXF_READ -- open for reading
pub const SSH_FXF_READ: u32 = 0x0000_0001;
/// SSH_FXF_WRITE -- open for writing
pub const SSH_FXF_WRITE: u32 = 0x0000_0002;
/// SSH_FXF_APPEND -- append on write
pub const SSH_FXF_APPEND: u32 = 0x0000_0004;
/// SSH_FXF_CREAT -- create if not exists
pub const SSH_FXF_CREAT: u32 = 0x0000_0008;
/// SSH_FXF_TRUNC -- truncate to zero length
pub const SSH_FXF_TRUNC: u32 = 0x0000_0010;
/// SSH_FXF_EXCL -- fail if already exists (combined with CREAT)
pub const SSH_FXF_EXCL: u32 = 0x0000_0020;

// =============================================================================
// File Attribute Flags
// =============================================================================

/// SSH_FILEXFER_ATTR_SIZE -- size field present
pub const SSH_FILEXFER_ATTR_SIZE: u32 = 0x0000_0001;
/// SSH_FILEXFER_ATTR_UIDGID -- uid/gid fields present
pub const SSH_FILEXFER_ATTR_UIDGID: u32 = 0x0000_0002;
/// SSH_FILEXFER_ATTR_PERMISSIONS -- permissions field present
pub const SSH_FILEXFER_ATTR_PERMISSIONS: u32 = 0x0000_0004;
/// SSH_FILEXFER_ATTR_ACMODTIME -- atime/mtime fields present
pub const SSH_FILEXFER_ATTR_ACMODTIME: u32 = 0x0000_0008;

// =============================================================================
// SftpFileAttrs -- file attribute structure
// =============================================================================

/// SFTP file attributes as sent on the wire (SSH_FXP_ATTRS).
///
/// Only fields whose flag bit is set in `flags` are valid; the rest are
/// `None`. This matches the SFTP wire format where only flagged fields
/// are transmitted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SftpFileAttrs {
    /// Attribute presence flags (SSH_FILEXFER_ATTR_*)
    pub flags: u32,
    /// File size in bytes
    pub size: Option<u64>,
    /// Owner user ID
    pub uid: Option<u32>,
    /// Owner group ID
    pub gid: Option<u32>,
    /// POSIX permission bits
    pub permissions: Option<u32>,
    /// Last access time (Unix epoch seconds)
    pub atime: Option<u32>,
    /// Last modification time (Unix epoch seconds)
    pub mtime: Option<u32>,
}

impl SftpFileAttrs {
    /// Create an `SftpFileAttrs` with all standard fields populated.
    pub fn full(size: u64, uid: u32, gid: u32, permissions: u32, atime: u32, mtime: u32) -> Self {
        Self {
            flags: SSH_FILEXFER_ATTR_SIZE
                | SSH_FILEXFER_ATTR_UIDGID
                | SSH_FILEXFER_ATTR_PERMISSIONS
                | SSH_FILEXFER_ATTR_ACMODTIME,
            size: Some(size),
            uid: Some(uid),
            gid: Some(gid),
            permissions: Some(permissions),
            atime: Some(atime),
            mtime: Some(mtime),
        }
    }

    /// Return the attribute flags word.
    pub fn flags(&self) -> u32 {
        self.flags
    }

    /// Check if the permissions indicate a directory (S_ISDIR).
    pub fn is_directory(&self) -> bool {
        self.permissions.is_some_and(|p| (p & 0o170000) == 0o040000)
    }

    /// Check if the permissions indicate a regular file (S_ISREG).
    pub fn is_regular_file(&self) -> bool {
        self.permissions.is_some_and(|p| (p & 0o170000) == 0o100000)
    }

    /// Check if the permissions indicate a symlink (S_ISLNK).
    pub fn is_symlink(&self) -> bool {
        self.permissions.is_some_and(|p| (p & 0o170000) == 0o120000)
    }

    /// Encode this attribute block into the writer (SFTP wire format).
    pub fn encode_to(&self, w: &mut impl Write) -> io::Result<()> {
        write_u32(w, self.flags)?;
        if self.flags & SSH_FILEXFER_ATTR_SIZE != 0 {
            write_u64(w, self.size.unwrap_or(0))?;
        }
        if self.flags & SSH_FILEXFER_ATTR_UIDGID != 0 {
            write_u32(w, self.uid.unwrap_or(0))?;
            write_u32(w, self.gid.unwrap_or(0))?;
        }
        if self.flags & SSH_FILEXFER_ATTR_PERMISSIONS != 0 {
            write_u32(w, self.permissions.unwrap_or(0))?;
        }
        if self.flags & SSH_FILEXFER_ATTR_ACMODTIME != 0 {
            write_u32(w, self.atime.unwrap_or(0))?;
            write_u32(w, self.mtime.unwrap_or(0))?;
        }
        Ok(())
    }

    /// Decode an attribute block from the reader.
    pub fn decode_from(r: &mut impl Read) -> io::Result<Self> {
        let flags = read_u32(r)?;
        let size = if flags & SSH_FILEXFER_ATTR_SIZE != 0 {
            Some(read_u64(r)?)
        } else {
            None
        };
        let (uid, gid) = if flags & SSH_FILEXFER_ATTR_UIDGID != 0 {
            (Some(read_u32(r)?), Some(read_u32(r)?))
        } else {
            (None, None)
        };
        let permissions = if flags & SSH_FILEXFER_ATTR_PERMISSIONS != 0 {
            Some(read_u32(r)?)
        } else {
            None
        };
        let (atime, mtime) = if flags & SSH_FILEXFER_ATTR_ACMODTIME != 0 {
            (Some(read_u32(r)?), Some(read_u32(r)?))
        } else {
            (None, None)
        };
        Ok(Self {
            flags,
            size,
            uid,
            gid,
            permissions,
            atime,
            mtime,
        })
    }
}

// =============================================================================
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

impl SftpPacket {
    /// Return the wire type code for this packet variant.
    pub fn packet_type(&self) -> u8 {
        match self {
            Self::Init { .. } => SSH_FXP_INIT,
            Self::Version { .. } => SSH_FXP_VERSION,
            Self::Open { .. } => SSH_FXP_OPEN,
            Self::Close { .. } => SSH_FXP_CLOSE,
            Self::Read { .. } => SSH_FXP_READ,
            Self::Write { .. } => SSH_FXP_WRITE,
            Self::Lstat { .. } => SSH_FXP_LSTAT,
            Self::Fstat { .. } => SSH_FXP_FSTAT,
            Self::Setstat { .. } => SSH_FXP_SETSTAT,
            Self::Fsetstat { .. } => SSH_FXP_FSETSTAT,
            Self::Opendir { .. } => SSH_FXP_OPENDIR,
            Self::Readdir { .. } => SSH_FXP_READDIR,
            Self::Remove { .. } => SSH_FXP_REMOVE,
            Self::Mkdir { .. } => SSH_FXP_MKDIR,
            Self::Rmdir { .. } => SSH_FXP_RMDIR,
            Self::Realpath { .. } => SSH_FXP_REALPATH,
            Self::Stat { .. } => SSH_FXP_STAT,
            Self::Rename { .. } => SSH_FXP_RENAME,
            Self::Readlink { .. } => SSH_FXP_READLINK,
            Self::Symlink { .. } => SSH_FXP_SYMLINK,
            Self::Status { .. } => SSH_FXP_STATUS,
            Self::Handle { .. } => SSH_FXP_HANDLE,
            Self::Data { .. } => SSH_FXP_DATA,
            Self::Name { .. } => SSH_FXP_NAME,
            Self::Attrs { .. } => SSH_FXP_ATTRS,
        }
    }

    /// Return the request_id if this packet carries one, else None.
    pub fn request_id(&self) -> Option<u32> {
        match self {
            Self::Init { .. } | Self::Version { .. } => None,
            Self::Open { request_id, .. }
            | Self::Close { request_id, .. }
            | Self::Read { request_id, .. }
            | Self::Write { request_id, .. }
            | Self::Lstat { request_id, .. }
            | Self::Fstat { request_id, .. }
            | Self::Setstat { request_id, .. }
            | Self::Fsetstat { request_id, .. }
            | Self::Opendir { request_id, .. }
            | Self::Readdir { request_id, .. }
            | Self::Remove { request_id, .. }
            | Self::Mkdir { request_id, .. }
            | Self::Rmdir { request_id, .. }
            | Self::Realpath { request_id, .. }
            | Self::Stat { request_id, .. }
            | Self::Rename { request_id, .. }
            | Self::Readlink { request_id, .. }
            | Self::Symlink { request_id, .. }
            | Self::Status { request_id, .. }
            | Self::Handle { request_id, .. }
            | Self::Data { request_id, .. }
            | Self::Name { request_id, .. }
            | Self::Attrs { request_id, .. } => Some(*request_id),
        }
    }

    /// Encode this packet into a byte vector ready for the SSH channel.
    ///
    /// Wire layout: `[length:u32][payload]`
    /// where payload starts with `type:u8` followed by type-specific fields.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let mut payload = Vec::with_capacity(64);
        payload.push(self.packet_type());

        match self {
            Self::Init { version } => {
                write_u32(&mut payload, *version)?;
            }
            Self::Version {
                version,
                extensions,
            } => {
                write_u32(&mut payload, *version)?;
                for (name, data) in extensions {
                    write_string(&mut payload, name)?;
                    write_string(&mut payload, data)?;
                }
            }
            Self::Open {
                request_id,
                filename,
                flags,
                attrs,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, filename)?;
                write_u32(&mut payload, *flags)?;
                attrs.encode_to(&mut payload)?;
            }
            Self::Close { request_id, handle } => {
                write_u32(&mut payload, *request_id)?;
                write_string_raw(&mut payload, handle)?;
            }
            Self::Read {
                request_id,
                handle,
                offset,
                length,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string_raw(&mut payload, handle)?;
                write_u64(&mut payload, *offset)?;
                write_u32(&mut payload, *length)?;
            }
            Self::Write {
                request_id,
                handle,
                offset,
                data,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string_raw(&mut payload, handle)?;
                write_u64(&mut payload, *offset)?;
                write_string_raw(&mut payload, data)?;
            }
            Self::Lstat { request_id, path } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, path)?;
            }
            Self::Fstat { request_id, handle } => {
                write_u32(&mut payload, *request_id)?;
                write_string_raw(&mut payload, handle)?;
            }
            Self::Setstat {
                request_id,
                path,
                attrs,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, path)?;
                attrs.encode_to(&mut payload)?;
            }
            Self::Fsetstat {
                request_id,
                handle,
                attrs,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string_raw(&mut payload, handle)?;
                attrs.encode_to(&mut payload)?;
            }
            Self::Opendir { request_id, path } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, path)?;
            }
            Self::Readdir { request_id, handle } => {
                write_u32(&mut payload, *request_id)?;
                write_string_raw(&mut payload, handle)?;
            }
            Self::Remove {
                request_id,
                filename,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, filename)?;
            }
            Self::Mkdir {
                request_id,
                path,
                attrs,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, path)?;
                attrs.encode_to(&mut payload)?;
            }
            Self::Rmdir { request_id, path } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, path)?;
            }
            Self::Realpath { request_id, path } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, path)?;
            }
            Self::Stat { request_id, path } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, path)?;
            }
            Self::Rename {
                request_id,
                old_path,
                new_path,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, old_path)?;
                write_string(&mut payload, new_path)?;
            }
            Self::Readlink { request_id, path } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, path)?;
            }
            Self::Symlink {
                request_id,
                link_path,
                target_path,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_string(&mut payload, link_path)?;
                write_string(&mut payload, target_path)?;
            }
            Self::Status {
                request_id,
                code,
                message,
                language,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_u32(&mut payload, *code)?;
                write_string(&mut payload, message)?;
                write_string(&mut payload, language)?;
            }
            Self::Handle { request_id, handle } => {
                write_u32(&mut payload, *request_id)?;
                write_string_raw(&mut payload, handle)?;
            }
            Self::Data { request_id, data } => {
                write_u32(&mut payload, *request_id)?;
                write_string_raw(&mut payload, data)?;
            }
            Self::Name {
                request_id,
                entries,
            } => {
                write_u32(&mut payload, *request_id)?;
                write_u32(&mut payload, entries.len() as u32)?;
                for entry in entries {
                    write_string(&mut payload, &entry.filename)?;
                    write_string(&mut payload, &entry.longname)?;
                    entry.attrs.encode_to(&mut payload)?;
                }
            }
            Self::Attrs { request_id, attrs } => {
                write_u32(&mut payload, *request_id)?;
                attrs.encode_to(&mut payload)?;
            }
        }

        // Wrap payload: [length:u32][payload]
        let mut out = Vec::with_capacity(4 + payload.len());
        write_u32(&mut out, payload.len() as u32)?;
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Decode a packet from a byte buffer.
    ///
    /// Returns `(packet, bytes_consumed)` so the caller can trim its buffer.
    /// If there are not enough bytes for a complete packet, returns
    /// `Err(io::ErrorKind::UnexpectedEof)`.
    pub fn decode(buf: &[u8]) -> io::Result<(Self, usize)> {
        if buf.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "need length prefix",
            ));
        }
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if buf.len() < 4 + len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete packet",
            ));
        }
        let payload = &buf[4..4 + len];
        let mut cursor = io::Cursor::new(payload);

        let pkt_type = read_u8(&mut cursor)?;
        let pkt = match pkt_type {
            SSH_FXP_INIT => {
                let version = read_u32(&mut cursor)?;
                Self::Init { version }
            }
            SSH_FXP_VERSION => {
                let version = read_u32(&mut cursor)?;
                let mut extensions = Vec::new();
                while cursor.position() < payload.len() as u64 {
                    let name = read_string(&mut cursor)?;
                    let data = read_string(&mut cursor)?;
                    extensions.push((name, data));
                }
                Self::Version {
                    version,
                    extensions,
                }
            }
            SSH_FXP_OPEN => {
                let request_id = read_u32(&mut cursor)?;
                let filename = read_string(&mut cursor)?;
                let flags = read_u32(&mut cursor)?;
                let attrs = SftpFileAttrs::decode_from(&mut cursor)?;
                Self::Open {
                    request_id,
                    filename,
                    flags,
                    attrs,
                }
            }
            SSH_FXP_CLOSE => {
                let request_id = read_u32(&mut cursor)?;
                let handle = read_raw(&mut cursor)?;
                Self::Close { request_id, handle }
            }
            SSH_FXP_READ => {
                let request_id = read_u32(&mut cursor)?;
                let handle = read_raw(&mut cursor)?;
                let offset = read_u64(&mut cursor)?;
                let length = read_u32(&mut cursor)?;
                Self::Read {
                    request_id,
                    handle,
                    offset,
                    length,
                }
            }
            SSH_FXP_WRITE => {
                let request_id = read_u32(&mut cursor)?;
                let handle = read_raw(&mut cursor)?;
                let offset = read_u64(&mut cursor)?;
                let data = read_raw(&mut cursor)?;
                Self::Write {
                    request_id,
                    handle,
                    offset,
                    data,
                }
            }
            SSH_FXP_LSTAT => {
                let request_id = read_u32(&mut cursor)?;
                let path = read_string(&mut cursor)?;
                Self::Lstat { request_id, path }
            }
            SSH_FXP_FSTAT => {
                let request_id = read_u32(&mut cursor)?;
                let handle = read_raw(&mut cursor)?;
                Self::Fstat { request_id, handle }
            }
            SSH_FXP_SETSTAT => {
                let request_id = read_u32(&mut cursor)?;
                let path = read_string(&mut cursor)?;
                let attrs = SftpFileAttrs::decode_from(&mut cursor)?;
                Self::Setstat {
                    request_id,
                    path,
                    attrs,
                }
            }
            SSH_FXP_FSETSTAT => {
                let request_id = read_u32(&mut cursor)?;
                let handle = read_raw(&mut cursor)?;
                let attrs = SftpFileAttrs::decode_from(&mut cursor)?;
                Self::Fsetstat {
                    request_id,
                    handle,
                    attrs,
                }
            }
            SSH_FXP_OPENDIR => {
                let request_id = read_u32(&mut cursor)?;
                let path = read_string(&mut cursor)?;
                Self::Opendir { request_id, path }
            }
            SSH_FXP_READDIR => {
                let request_id = read_u32(&mut cursor)?;
                let handle = read_raw(&mut cursor)?;
                Self::Readdir { request_id, handle }
            }
            SSH_FXP_REMOVE => {
                let request_id = read_u32(&mut cursor)?;
                let filename = read_string(&mut cursor)?;
                Self::Remove {
                    request_id,
                    filename,
                }
            }
            SSH_FXP_MKDIR => {
                let request_id = read_u32(&mut cursor)?;
                let path = read_string(&mut cursor)?;
                let attrs = SftpFileAttrs::decode_from(&mut cursor)?;
                Self::Mkdir {
                    request_id,
                    path,
                    attrs,
                }
            }
            SSH_FXP_RMDIR => {
                let request_id = read_u32(&mut cursor)?;
                let path = read_string(&mut cursor)?;
                Self::Rmdir { request_id, path }
            }
            SSH_FXP_REALPATH => {
                let request_id = read_u32(&mut cursor)?;
                let path = read_string(&mut cursor)?;
                Self::Realpath { request_id, path }
            }
            SSH_FXP_STAT => {
                let request_id = read_u32(&mut cursor)?;
                let path = read_string(&mut cursor)?;
                Self::Stat { request_id, path }
            }
            SSH_FXP_RENAME => {
                let request_id = read_u32(&mut cursor)?;
                let old_path = read_string(&mut cursor)?;
                let new_path = read_string(&mut cursor)?;
                Self::Rename {
                    request_id,
                    old_path,
                    new_path,
                }
            }
            SSH_FXP_READLINK => {
                let request_id = read_u32(&mut cursor)?;
                let path = read_string(&mut cursor)?;
                Self::Readlink { request_id, path }
            }
            SSH_FXP_SYMLINK => {
                let request_id = read_u32(&mut cursor)?;
                let link_path = read_string(&mut cursor)?;
                let target_path = read_string(&mut cursor)?;
                Self::Symlink {
                    request_id,
                    link_path,
                    target_path,
                }
            }
            SSH_FXP_STATUS => {
                let request_id = read_u32(&mut cursor)?;
                let code = read_u32(&mut cursor)?;
                let message = read_string(&mut cursor)?;
                let language = read_string(&mut cursor)?;
                Self::Status {
                    request_id,
                    code,
                    message,
                    language,
                }
            }
            SSH_FXP_HANDLE => {
                let request_id = read_u32(&mut cursor)?;
                let handle = read_raw(&mut cursor)?;
                Self::Handle { request_id, handle }
            }
            SSH_FXP_DATA => {
                let request_id = read_u32(&mut cursor)?;
                let data = read_raw(&mut cursor)?;
                Self::Data { request_id, data }
            }
            SSH_FXP_NAME => {
                let request_id = read_u32(&mut cursor)?;
                let count = read_u32(&mut cursor)? as usize;
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let filename = read_string(&mut cursor)?;
                    let longname = read_string(&mut cursor)?;
                    let attrs = SftpFileAttrs::decode_from(&mut cursor)?;
                    entries.push(SftpNameEntry {
                        filename,
                        longname,
                        attrs,
                    });
                }
                Self::Name {
                    request_id,
                    entries,
                }
            }
            SSH_FXP_ATTRS => {
                let request_id = read_u32(&mut cursor)?;
                let attrs = SftpFileAttrs::decode_from(&mut cursor)?;
                Self::Attrs { request_id, attrs }
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Unknown SFTP packet type: {}", other),
                ));
            }
        };

        Ok((pkt, 4 + len))
    }
}

// =============================================================================
// Wire Helper Functions
// =============================================================================

fn write_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Write a UTF-8 string as `[length:u32][bytes]` (SFTP string format).
fn write_string(w: &mut impl Write, s: &str) -> io::Result<()> {
    write_u32(w, s.len() as u32)?;
    w.write_all(s.as_bytes())
}

/// Write raw bytes as `[length:u32][data]` (SFTP string/buffer format).
fn write_string_raw(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write_u32(w, data.len() as u32)?;
    w.write_all(data)
}

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

/// Read a UTF-8 string in `[length:u32][bytes]` format.
fn read_string(r: &mut impl Read) -> io::Result<String> {
    let len = read_u32(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read raw bytes in `[length:u32][data]` format.
fn read_raw(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let len = read_u32(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_roundtrip() {
        let pkt = SftpPacket::Init { version: 3 };
        let encoded = pkt.encode().unwrap();
        let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn test_version_with_extensions_roundtrip() {
        let pkt = SftpPacket::Version {
            version: 3,
            extensions: vec![
                ("hardlink@openssh.com".to_string(), "1".to_string()),
                ("fsync@openssh.com".to_string(), "1".to_string()),
            ],
        };
        let encoded = pkt.encode().unwrap();
        let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn test_open_roundtrip() {
        let pkt = SftpPacket::Open {
            request_id: 42,
            filename: "/tmp/test.dat".to_string(),
            flags: SSH_FXF_READ | SSH_FXF_WRITE,
            attrs: SftpFileAttrs::default(),
        };
        let encoded = pkt.encode().unwrap();
        let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn test_status_roundtrip() {
        let pkt = SftpPacket::Status {
            request_id: 1,
            code: SSH_FX_OK,
            message: "OK".to_string(),
            language: "en".to_string(),
        };
        let encoded = pkt.encode().unwrap();
        let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn test_handle_roundtrip() {
        let pkt = SftpPacket::Handle {
            request_id: 5,
            handle: vec![0x01, 0x02, 0x03],
        };
        let encoded = pkt.encode().unwrap();
        let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn test_data_roundtrip() {
        let pkt = SftpPacket::Data {
            request_id: 10,
            data: b"hello world".to_vec(),
        };
        let encoded = pkt.encode().unwrap();
        let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn test_name_roundtrip() {
        let pkt = SftpPacket::Name {
            request_id: 20,
            entries: vec![SftpNameEntry {
                filename: "file1.txt".to_string(),
                longname: "-rw-r--r-- 1 user group 1234 Jan 1 00:00 file1.txt".to_string(),
                attrs: SftpFileAttrs::full(1234, 1000, 1000, 0o100644, 1700000000, 1700000100),
            }],
        };
        let encoded = pkt.encode().unwrap();
        let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn test_attrs_roundtrip() {
        let pkt = SftpPacket::Attrs {
            request_id: 30,
            attrs: SftpFileAttrs::full(4096, 0, 0, 0o040755, 1700000000, 1700000100),
        };
        let encoded = pkt.encode().unwrap();
        let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn test_decode_incomplete_returns_error() {
        let buf = [0u8; 3]; // Too short for length prefix
        let result = SftpPacket::decode(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_partial_payload_returns_error() {
        let pkt = SftpPacket::Init { version: 3 };
        let encoded = pkt.encode().unwrap();
        let result = SftpPacket::decode(&encoded[..5]); // Only length + 1 byte
        assert!(result.is_err());
    }

    #[test]
    fn test_packet_type_codes() {
        assert_eq!(SftpPacket::Init { version: 3 }.packet_type(), SSH_FXP_INIT);
        assert_eq!(
            SftpPacket::Version {
                version: 3,
                extensions: vec![]
            }
            .packet_type(),
            SSH_FXP_VERSION
        );
        assert_eq!(
            SftpPacket::Open {
                request_id: 0,
                filename: String::new(),
                flags: 0,
                attrs: SftpFileAttrs::default(),
            }
            .packet_type(),
            SSH_FXP_OPEN
        );
    }

    #[test]
    fn test_request_id_extraction() {
        assert!(SftpPacket::Init { version: 3 }.request_id().is_none());
        assert!(
            SftpPacket::Version {
                version: 3,
                extensions: vec![]
            }
            .request_id()
            .is_none()
        );
        assert_eq!(
            SftpPacket::Open {
                request_id: 42,
                filename: String::new(),
                flags: 0,
                attrs: SftpFileAttrs::default(),
            }
            .request_id(),
            Some(42)
        );
        assert_eq!(
            SftpPacket::Status {
                request_id: 99,
                code: SSH_FX_OK,
                message: String::new(),
                language: String::new(),
            }
            .request_id(),
            Some(99)
        );
    }

    #[test]
    fn test_file_attrs_directory_check() {
        let dir_attrs = SftpFileAttrs::full(4096, 0, 0, 0o040755, 0, 0);
        assert!(dir_attrs.is_directory());
        assert!(!dir_attrs.is_regular_file());

        let file_attrs = SftpFileAttrs::full(100, 0, 0, 0o100644, 0, 0);
        assert!(file_attrs.is_regular_file());
        assert!(!file_attrs.is_directory());

        let link_attrs = SftpFileAttrs::full(10, 0, 0, 0o120777, 0, 0);
        assert!(link_attrs.is_symlink());
    }

    #[test]
    fn test_status_code_descriptions() {
        assert_eq!(status_code_description(SSH_FX_OK), "Operation succeeded");
        assert_eq!(status_code_description(SSH_FX_EOF), "End of file");
        assert_eq!(status_code_description(999), "Unknown status code");
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let mut buf = Vec::new();
        write_u32(&mut buf, 0xDEADBEEF).unwrap();
        write_u64(&mut buf, 0xCAFEBABE_CAFEBABE).unwrap();
        write_string(&mut buf, "hello").unwrap();
        write_string_raw(&mut buf, &[0x01, 0x02]).unwrap();

        let mut cursor = io::Cursor::new(&buf);
        assert_eq!(read_u32(&mut cursor).unwrap(), 0xDEADBEEF);
        assert_eq!(read_u64(&mut cursor).unwrap(), 0xCAFEBABE_CAFEBABE);
        assert_eq!(read_string(&mut cursor).unwrap(), "hello");
        assert_eq!(read_raw(&mut cursor).unwrap(), vec![0x01, 0x02]);
    }
}
