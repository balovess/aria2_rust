//! SFTP packet wire encoding and decoding.

use std::io;

use super::attrs::SftpFileAttrs;
use super::constants::*;
use super::types::{SftpNameEntry, SftpPacket};
use super::wire::{
    read_raw, read_string, read_u8, read_u32, read_u64, write_string, write_string_raw, write_u32,
    write_u64,
};
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
