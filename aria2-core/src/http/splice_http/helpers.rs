//! Internal helper functions for splice HTTP download (Linux only).
//!
//! These functions handle low-level HTTP response parsing and file I/O
//! operations used by the splice download path.

#[cfg(target_os = "linux")]
use std::io;

/// Find the end of HTTP headers (`\r\n\r\n`) in a byte buffer.
///
/// Returns the byte index of the first `\r` of the terminating `\r\n\r\n`
/// sequence, or `None` if the sequence is not present.
#[cfg(target_os = "linux")]
pub(crate) fn find_header_end(buf: &[u8]) -> Option<usize> {
    // We need at least 4 bytes to match \r\n\r\n.
    if buf.len() < 4 {
        return None;
    }
    // Search from the end of the previously-unsearched region. A simple
    // sliding window is sufficient — headers are small (< 8 KB).
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse the HTTP status code from the first line of the response.
///
/// Expected format: `HTTP/1.1 206 Partial Content\r\n`
#[cfg(target_os = "linux")]
pub(crate) fn parse_status_code(header_str: &str) -> io::Result<u16> {
    let first_line = header_str
        .lines()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty response"))?;

    let mut parts = first_line.split_whitespace();
    let _version = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP version"))?;
    let code_str = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing status code"))?;

    code_str.parse::<u16>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid status code: {code_str}"),
        )
    })
}

/// Check if the response uses `Transfer-Encoding: chunked`.
///
/// Case-insensitive header name and value matching.
#[cfg(target_os = "linux")]
pub(crate) fn is_chunked(header_str: &str) -> bool {
    for line in header_str.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("transfer-encoding")
            && value.trim().eq_ignore_ascii_case("chunked")
        {
            return true;
        }
    }
    false
}

/// Parse the `Content-Length` header value.
///
/// Returns `Ok(Some(length))` if found, `Ok(None)` if not present.
#[cfg(target_os = "linux")]
pub(crate) fn parse_content_length(header_str: &str) -> io::Result<Option<u64>> {
    for line in header_str.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let value = value.trim();
            return value.parse::<u64>().map(Some).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid Content-Length: {value}"),
                )
            });
        }
    }
    Ok(None)
}

/// Write all bytes at a specific offset in the file, looping to handle
/// partial writes.
///
/// Uses `pwrite(2)` via `FileExt::write_at` on Unix. The file cursor is not
/// modified.
#[cfg(target_os = "linux")]
pub(crate) fn write_all_at_offset(
    file: &std::fs::File,
    mut buf: &[u8],
    mut offset: u64,
) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        let n = file.write_at(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "pwrite returned 0 — failed to write all bytes",
            ));
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

/// Set a file descriptor to blocking mode by clearing `O_NONBLOCK`.
///
/// # Safety
///
/// The fd must be a valid open file descriptor. This function is called from
/// a `spawn_blocking` task where the fd is kept alive by the owning object
/// in the async context.
#[cfg(target_os = "linux")]
pub(crate) fn set_blocking(fd: std::os::unix::io::RawFd) {
    // SAFETY: fcntl with F_GETFL/F_SETFL is safe for a valid open fd. The fd
    // is owned by a tokio::net::TcpStream in the parent async task, which is
    // alive for the duration of this blocking task (we await its completion).
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}
