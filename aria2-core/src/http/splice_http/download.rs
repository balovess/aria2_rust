//! Linux-only zero-copy HTTP download via `splice(2)`.
//!
//! When downloading a file over plain HTTP (no HTTPS, no proxy) on Linux,
//! this module bypasses the hyper/reqwest HTTP client and uses a raw TCP
//! connection to enable `splice(2)` zero-copy transfer from socket to file.
//!
//! The response headers are read into user space (small, ~1 KB), then the
//! response body is spliced directly from the kernel socket buffer to the
//! output file via a pipe buffer — no user-space data copy for the body.
//!
//! # Limitations
//! - Linux only (splice is a Linux-specific syscall)
//! - Plain HTTP only (no TLS/HTTPS support)
//! - No proxy support
//! - No custom headers or cookies (use the reqwest path for those)
//! - HTTP 1.1 only (no HTTP/2)
//! - Requires `206 Partial Content` response (Range request)
//! - No chunked transfer encoding (Content-Length required)

use std::io;

#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "linux")]
use tracing::debug;

#[cfg(target_os = "linux")]
use crate::util::zero_copy::splice_transfer;

#[cfg(target_os = "linux")]
use super::helpers::{
    find_header_end, is_chunked, parse_content_length, parse_status_code, set_blocking,
    write_all_at_offset,
};

/// Maximum size of the response header buffer. Headers should be much smaller
/// (~1 KB typical), but we allow up to 8 KB to accommodate servers that send
/// large Set-Cookie or custom headers.
#[cfg(target_os = "linux")]
const MAX_HEADER_SIZE: usize = 8 * 1024;

/// Attempt a zero-copy splice download of a byte range from `url` into `file`
/// at `file_offset`.
///
/// This function creates its own TCP connection, sends a raw HTTP/1.1 GET
/// request with a Range header, reads the response headers into a small
/// user-space buffer, then uses `splice(2)` to transfer the response body
/// directly from the kernel socket buffer to the output file — no user-space
/// data copy for the body.
///
/// # Arguments
///
/// * `url` - Plain HTTP URL to download from (must be `http://`, not `https://`).
/// * `offset` - Byte offset for the Range request (`bytes=offset-...`).
/// * `length` - Number of bytes requested in the Range.
/// * `file` - Output file open for writing. Must be a regular file (splice
///   cannot target sockets or pipes via this helper).
/// * `file_offset` - Offset within the output file to write the body at.
///
/// # Returns
///
/// `Ok(bytes_transferred)` on success. The caller should verify the byte
/// count matches expectations.
///
/// `Err` on any failure — the caller should fall back to the standard
/// reqwest/hyper download path.
#[cfg(target_os = "linux")]
pub async fn try_splice_download(
    url: &str,
    offset: u64,
    length: u64,
    file: &std::fs::File,
    file_offset: u64,
) -> io::Result<u64> {
    if length == 0 {
        return Ok(0);
    }

    // 1. Parse URL — extract host, port, path.
    let parsed = url::Url::parse(url)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid URL: {e}")))?;

    if parsed.scheme() != "http" {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "splice requires plain HTTP (no HTTPS)",
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URL has no host"))?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    };
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
    let path_query = format!("{path}{query}");

    // 2. DNS resolution via tokio's async resolver.
    let addr = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("DNS resolution failed: {e}"),
            )
        })?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no addresses resolved"))?;

    debug!(host, port, %addr, "splice_download: connecting");

    // 3. TCP connect.
    let mut stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
        io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("TCP connect failed: {e}"),
        )
    })?;
    // Disable Nagle's algorithm — we send the full request at once and want
    // the response without delay.
    let _ = stream.set_nodelay(true);

    // 4. Send raw HTTP/1.1 GET request with Range header.
    let host_header = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    let range_value = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
    let request = format!(
        "GET {path_query} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         Range: {range_value}\r\n\
         Connection: close\r\n\
         User-Agent: {}\r\n\
         Accept: */*\r\n\
         \r\n",
        crate::constants::USER_AGENT,
    );
    stream.write_all(request.as_bytes()).await?;
    debug!(range = %range_value, "splice_download: request sent");

    // 5. Read response headers into a buffer until we find \r\n\r\n.
    let mut header_buf = vec![0u8; MAX_HEADER_SIZE];
    let mut header_len = 0usize;
    let header_end_pos = loop {
        if header_len >= MAX_HEADER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response headers exceed 8 KB",
            ));
        }
        let n = stream.read(&mut header_buf[header_len..]).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before headers complete",
            ));
        }
        header_len += n;
        if let Some(pos) = find_header_end(&header_buf[..header_len]) {
            break pos;
        }
    };

    // 6. Parse status code — must be 206 (Partial Content).
    //    Any other status (200, 416, 4xx, 5xx) → fall back to reqwest.
    let header_bytes = &header_buf[..header_end_pos];
    let header_str = std::str::from_utf8(header_bytes).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("non-UTF8 headers: {e}"))
    })?;
    let status = parse_status_code(header_str)?;
    if status != 206 {
        return Err(io::Error::other(format!(
            "expected 206 Partial Content, got {status}"
        )));
    }

    // 7. Parse Content-Length; reject chunked encoding (splice can't handle it).
    if is_chunked(header_str) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "chunked transfer encoding not supported by splice",
        ));
    }
    let content_length = parse_content_length(header_str)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;

    if content_length > length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Content-Length ({content_length}) exceeds requested length ({length})"),
        ));
    }

    // 8. Write pre-read body bytes (after \r\n\r\n in the header buffer) to
    //    the file at file_offset using positioned write (pwrite).
    let body_start = header_end_pos + 4; // skip the 4-byte \r\n\r\n delimiter
    let pre_read = &header_buf[body_start..header_len];
    let pre_read_len = pre_read.len() as u64;

    if pre_read_len > content_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "pre-read body bytes ({pre_read_len}) exceed Content-Length ({content_length})"
            ),
        ));
    }

    if pre_read_len > 0 {
        write_all_at_offset(file, pre_read, file_offset)?;
    }

    let mut written = pre_read_len;
    let remaining = content_length - pre_read_len;

    // 9. Splice remaining bytes from socket fd to file fd.
    //    splice_transfer loops internally (64 KiB chunks) until all `remaining`
    //    bytes are transferred or EOF is reached. We run it in a blocking task
    //    because splice(2) on a blocking socket waits for network data — this
    //    must not stall the tokio worker thread.
    if remaining > 0 {
        let socket_fd = stream.as_raw_fd();
        let file_fd = file.as_raw_fd();
        let splice_file_offset = (file_offset + pre_read_len) as i64;
        let splice_len = remaining as usize;

        let splice_result = tokio::task::spawn_blocking(move || {
            // Switch the socket to blocking mode so splice(2) blocks until
            // data arrives instead of returning EAGAIN. This is safe because
            // we no longer perform async I/O on this socket — the header read
            // is done, and the socket will be dropped after splice completes.
            set_blocking(socket_fd);
            splice_transfer(
                socket_fd,
                None,
                file_fd,
                Some(splice_file_offset),
                splice_len,
            )
        })
        .await
        .map_err(|e| io::Error::other(format!("blocking task failed: {e}")))?
        .map_err(|e| io::Error::other(format!("splice failed: {e}")))?;

        let spliced = splice_result as u64;
        if spliced < remaining {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "splice EOF: transferred {} of {} body bytes (pre_read={}, spliced={})",
                    written + spliced,
                    content_length,
                    pre_read_len,
                    spliced
                ),
            ));
        }
        written += spliced;
    }

    debug!(
        url,
        bytes = written,
        pre_read = pre_read_len,
        spliced = written - pre_read_len,
        "splice_download: complete"
    );

    // 10. Return total bytes transferred.
    Ok(written)
}

/// Non-Linux stub — always returns `Err(Unsupported)`.
#[cfg(not(target_os = "linux"))]
pub async fn try_splice_download(
    _url: &str,
    _offset: u64,
    _length: u64,
    _file: &std::fs::File,
    _file_offset: u64,
) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "splice not available on this platform",
    ))
}
