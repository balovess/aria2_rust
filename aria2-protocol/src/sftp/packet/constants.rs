//! SFTP protocol message and attribute constants.

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
