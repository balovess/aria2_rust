//! Primitive SFTP wire field encoders and decoders.

use std::io::{self, Read, Write};
pub(super) fn write_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub(super) fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Write a UTF-8 string as `[length:u32][bytes]` (SFTP string format).
pub(super) fn write_string(w: &mut impl Write, s: &str) -> io::Result<()> {
    write_u32(w, s.len() as u32)?;
    w.write_all(s.as_bytes())
}

/// Write raw bytes as `[length:u32][data]` (SFTP string/buffer format).
pub(super) fn write_string_raw(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write_u32(w, data.len() as u32)?;
    w.write_all(data)
}

pub(super) fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

pub(super) fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

pub(super) fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

/// Read a UTF-8 string in `[length:u32][bytes]` format.
pub(super) fn read_string(r: &mut impl Read) -> io::Result<String> {
    let len = read_u32(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read raw bytes in `[length:u32][data]` format.
pub(super) fn read_raw(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let len = read_u32(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// =============================================================================
