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
    aria2_protocol::http::header_processor::find_header_end(buf)
}

#[cfg(target_os = "linux")]
fn parse_header(
    header_str: &str,
) -> io::Result<aria2_protocol::http::header_processor::HttpResponseHead> {
    let complete_header = if header_str.ends_with("\r\n\r\n") {
        header_str.to_owned()
    } else if header_str.ends_with("\r\n") {
        format!("{header_str}\r\n")
    } else {
        format!("{header_str}\r\n\r\n")
    };
    aria2_protocol::http::header_processor::parse_response_head(complete_header.as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Parse the HTTP status code from the first line of the response.
///
/// Expected format: `HTTP/1.1 206 Partial Content\r\n`
#[cfg(target_os = "linux")]
pub(crate) fn parse_status_code(header_str: &str) -> io::Result<u16> {
    Ok(parse_header(header_str)?.status_code)
}

/// Check if the response uses `Transfer-Encoding: chunked`.
///
/// Case-insensitive header name and value matching.
#[cfg(target_os = "linux")]
pub(crate) fn is_chunked(header_str: &str) -> bool {
    parse_header(header_str)
        .ok()
        .and_then(|head| head.header("transfer-encoding").map(str::to_owned))
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("chunked"))
}

/// Parse the `Content-Length` header value.
///
/// Returns `Ok(Some(length))` if found, `Ok(None)` if not present.
#[cfg(target_os = "linux")]
pub(crate) fn parse_content_length(header_str: &str) -> io::Result<Option<u64>> {
    parse_header(header_str)?
        .header("content-length")
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .transpose()
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
