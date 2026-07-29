//! Platform-specific positioned I/O helpers.
//!
//! Provides `write_all_at` and `read_exact_at` functions that wrap the
//! OS-native positioned I/O syscalls, handling partial writes/reads.

use crate::error::{Aria2Error, Result};

/// Positioned write that loops to handle partial writes.
///
/// Writes the entire `buf` at `offset` without modifying the file cursor,
/// preserving `pwrite(2)` semantics while guaranteeing a complete write.
pub(super) fn write_all_at(file: &std::fs::File, mut buf: &[u8], mut offset: u64) -> Result<()> {
    while !buf.is_empty() {
        let n = positioned_write(file, buf, offset)?;
        if n == 0 {
            return Err(Aria2Error::Io(
                "positioned write returned 0 — failed to write whole buffer".into(),
            ));
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

/// Positioned read that loops to fill as much of `buf` as possible.
///
/// Returns the number of bytes read (may be less than `buf.len()` at EOF).
pub(super) fn read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
    let mut filled = 0usize;
    let mut current_offset = offset;
    while filled < buf.len() {
        let n = positioned_read(file, &mut buf[filled..], current_offset)?;
        if n == 0 {
            break; // EOF reached
        }
        filled += n;
        current_offset += n as u64;
    }
    Ok(filled)
}

/// Single positioned write syscall. Returns bytes written.
#[cfg(unix)]
fn positioned_write(file: &std::fs::File, buf: &[u8], offset: u64) -> Result<usize> {
    use std::os::unix::fs::FileExt;
    Ok(file.write_at(buf, offset)?)
}

/// Single positioned read syscall. Returns bytes read.
#[cfg(unix)]
fn positioned_read(file: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
    use std::os::unix::fs::FileExt;
    Ok(file.read_at(buf, offset)?)
}

/// Single positioned write syscall (Windows). Returns bytes written.
#[cfg(windows)]
fn positioned_write(file: &std::fs::File, buf: &[u8], offset: u64) -> Result<usize> {
    use std::os::windows::fs::FileExt;
    Ok(file.seek_write(buf, offset)?)
}

/// Single positioned read syscall (Windows). Returns bytes read.
#[cfg(windows)]
fn positioned_read(file: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
    use std::os::windows::fs::FileExt;
    Ok(file.seek_read(buf, offset)?)
}

#[cfg(not(any(unix, windows)))]
fn positioned_write(_file: &std::fs::File, _buf: &[u8], _offset: u64) -> Result<usize> {
    Err(Aria2Error::Io(
        "positioned write not supported on this platform".into(),
    ))
}

#[cfg(not(any(unix, windows)))]
fn positioned_read(_file: &std::fs::File, _buf: &mut [u8], _offset: u64) -> Result<usize> {
    Err(Aria2Error::Io(
        "positioned read not supported on this platform".into(),
    ))
}