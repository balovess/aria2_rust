//! SFTP wire attributes.

use std::io::{self, Read, Write};

use super::constants::*;
use super::wire::{read_u32, read_u64, write_u32, write_u64};
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
