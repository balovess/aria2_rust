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

    let host = parsed.host_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "URL has no host")
    })?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let path = if parsed.path().is_empty() { "/" } else { parsed.path() };
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
    let path_query = format!("{path}{query}");

    // 2. DNS resolution via tokio's async resolver.
    let addr = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::AddrNotAvailable, format!("DNS resolution failed: {e}")))?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no addresses resolved"))?;

    debug!(host, port, %addr, "splice_download: connecting");

    // 3. TCP connect.
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, format!("TCP connect failed: {e}")))?;
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
         User-Agent: aria2-rust/1.0\r\n\
         Accept: */*\r\n\
         \r\n",
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
    let header_str = std::str::from_utf8(header_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("non-UTF8 headers: {e}")))?;
    let status = parse_status_code(header_str)?;
    if status != 206 {
        return Err(io::Error::new(
            io::ErrorKind::Other, // "Other" so caller falls back; reqwest handles 200/416/etc.
            format!("expected 206 Partial Content, got {status}"),
        ));
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
            format!(
                "Content-Length ({content_length}) exceeds requested length ({length})"
            ),
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
            splice_transfer(socket_fd, None, file_fd, Some(splice_file_offset), splice_len)
        })
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("blocking task failed: {e}")))?
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("splice failed: {e}")))?;

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

// =========================================================================
// Internal helpers (Linux only)
// =========================================================================

/// Find the end of HTTP headers (`\r\n\r\n`) in a byte buffer.
///
/// Returns the byte index of the first `\r` of the terminating `\r\n\r\n`
/// sequence, or `None` if the sequence is not present.
#[cfg(target_os = "linux")]
fn find_header_end(buf: &[u8]) -> Option<usize> {
    // We need at least 4 bytes to match \r\n\r\n.
    if buf.len() < 4 {
        return None;
    }
    // Search from the end of the previously-unsearched region. A simple
    // sliding window is sufficient — headers are small (< 8 KB).
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
}

/// Parse the HTTP status code from the first line of the response.
///
/// Expected format: `HTTP/1.1 206 Partial Content\r\n`
#[cfg(target_os = "linux")]
fn parse_status_code(header_str: &str) -> io::Result<u16> {
    let first_line = header_str
        .lines()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty response"))?;

    let mut parts = first_line.split_whitespace();
    let _version = parts.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing HTTP version")
    })?;
    let code_str = parts.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing status code")
    })?;

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
fn is_chunked(header_str: &str) -> bool {
    for line in header_str.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.trim().eq_ignore_ascii_case("chunked")
            {
                return true;
            }
        }
    }
    false
}

/// Parse the `Content-Length` header value.
///
/// Returns `Ok(Some(length))` if found, `Ok(None)` if not present.
#[cfg(target_os = "linux")]
fn parse_content_length(header_str: &str) -> io::Result<Option<u64>> {
    for line in header_str.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                let value = value.trim();
                return value.parse::<u64>().map(Some).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid Content-Length: {value}"),
                    )
                });
            }
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
fn write_all_at_offset(file: &std::fs::File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
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
fn set_blocking(fd: std::os::unix::io::RawFd) {
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

// =========================================================================
// Tests (Linux only)
// =========================================================================

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Spawn a mock HTTP server that responds to a Range request with 206
    /// Partial Content. Returns the server address.
    ///
    /// The server sends `body[offset..offset+length]` as the response body
    /// with a Content-Length header.
    async fn spawn_mock_206_server(body: Vec<u8>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();

            // Read the request (until \r\n\r\n).
            let mut req_buf = [0u8; 4096];
            let _n = sock.read(&mut req_buf).await.unwrap();

            // Parse the Range header from the request to determine what to send.
            let req_str = std::str::from_utf8(&req_buf).unwrap_or("");
            let (offset, length) = parse_request_range(req_str, body.len());

            let end = offset + length.saturating_sub(1);
            let chunk = &body[offset..offset + length.min(body.len() - offset)];

            let response = format!(
                "HTTP/1.1 206 Partial Content\r\n\
                 Content-Length: {}\r\n\
                 Content-Range: bytes {}-{}/{}\r\n\
                 Connection: close\r\n\
                 \r\n",
                chunk.len(),
                offset,
                end,
                body.len()
            );

            sock.write_all(response.as_bytes()).await.unwrap();
            sock.write_all(chunk).await.unwrap();
        });

        addr
    }

    /// Parse `Range: bytes=START-END` from a raw HTTP request string.
    fn parse_request_range(req: &str, total: usize) -> (usize, usize) {
        for line in req.lines() {
            if let Some(rest) = line.strip_prefix("Range:") {
                let rest = rest.trim();
                if let Some(range) = rest.strip_prefix("bytes=") {
                    if let Some((start_s, end_s)) = range.split_once('-') {
                        let start: usize = start_s.parse().unwrap_or(0);
                        let end: usize = end_s.parse().unwrap_or(total - 1);
                        let length = end.saturating_sub(start) + 1;
                        return (start, length);
                    }
                }
            }
        }
        (0, total)
    }

    #[tokio::test]
    async fn test_splice_download_basic() {
        // Create a payload and serve it via a mock HTTP server.
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 256) as u8).collect();
        let addr = spawn_mock_206_server(payload.clone()).await;

        // Create output file.
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("out.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();

        let url = format!("http://{addr}/test.bin");
        let n = try_splice_download(&url, 0, payload.len() as u64, &file, 0)
            .await
            .expect("splice download should succeed");

        assert_eq!(n, payload.len() as u64);

        // Verify the file content matches the payload.
        drop(file);
        let content = std::fs::read(&out_path).unwrap();
        assert_eq!(content, payload);
    }

    #[tokio::test]
    async fn test_splice_download_range_offset() {
        // Download a sub-range starting at a non-zero offset.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
        let addr = spawn_mock_206_server(payload.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("out_range.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();

        let offset = 50_000u64;
        let length = 80_000u64;
        let url = format!("http://{addr}/test.bin");
        let n = try_splice_download(&url, offset, length, &file, 0)
            .await
            .expect("splice range download should succeed");

        assert_eq!(n, length);

        // Verify the file content matches the requested sub-range.
        drop(file);
        let content = std::fs::read(&out_path).unwrap();
        assert_eq!(content.len(), length as usize);
        assert_eq!(content, &payload[offset as usize..(offset + length) as usize]);
    }

    #[tokio::test]
    async fn test_splice_download_with_file_offset() {
        // Splice data at a non-zero file offset (concurrent segment scenario).
        let payload: Vec<u8> = (0..64_000u32).map(|i| (i % 256) as u8).collect();
        let addr = spawn_mock_206_server(payload.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("out_foff.bin");
        // Pre-allocate a larger file so pwrite at offset works.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        file.set_len(100_000).unwrap();

        let file_offset = 30_000u64;
        let url = format!("http://{addr}/test.bin");
        let n = try_splice_download(&url, 0, payload.len() as u64, &file, file_offset)
            .await
            .expect("splice with file offset should succeed");

        assert_eq!(n, payload.len() as u64);

        // Verify the data was written at the correct offset.
        drop(file);
        let content = std::fs::read(&out_path).unwrap();
        assert_eq!(content.len(), 100_000);
        // The region before file_offset should be zero-filled.
        assert!(
            content[..file_offset as usize].iter().all(|&b| b == 0),
            "region before file_offset should be zero"
        );
        // The spliced region should match the payload.
        assert_eq!(
            &content[file_offset as usize..file_offset as usize + payload.len()],
            &payload[..]
        );
        // The region after should be zero-filled.
        assert!(
            content[file_offset as usize + payload.len()..]
                .iter()
                .all(|&b| b == 0),
            "region after spliced data should be zero"
        );
    }

    #[tokio::test]
    async fn test_splice_download_https_rejected() {
        let file = tempfile::tempfile().unwrap();
        let result = try_splice_download("https://example.com/file", 0, 100, &file, 0).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn test_splice_download_zero_length() {
        let file = tempfile::tempfile().unwrap();
        let result = try_splice_download("http://example.com/file", 0, 0, &file, 0).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_splice_download_non_206_falls_back() {
        // Server returns 200 (not 206) — splice should return Err so the
        // caller falls back to reqwest.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
                .await
                .unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("out_200.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();

        let url = format!("http://{addr}/test.bin");
        let result = try_splice_download(&url, 0, 5, &file, 0).await;
        assert!(result.is_err(), "non-206 should return Err for fallback");
    }

    #[tokio::test]
    async fn test_splice_download_small_body_in_header_buffer() {
        // When the body fits entirely in the header read buffer (pre-read
        // path), verify it's written correctly via pwrite.
        let payload = b"tiny payload!".to_vec();
        let addr = spawn_mock_206_server(payload.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("out_tiny.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();

        let url = format!("http://{addr}/test.bin");
        let n = try_splice_download(&url, 0, payload.len() as u64, &file, 0)
            .await
            .expect("splice tiny download should succeed");

        assert_eq!(n, payload.len() as u64);

        drop(file);
        let content = std::fs::read(&out_path).unwrap();
        assert_eq!(content, payload);
    }

    #[test]
    fn test_find_header_end() {
        assert_eq!(find_header_end(b""), None);
        assert_eq!(find_header_end(b"HTTP/1.1 200 OK\r\n"), None);
        assert_eq!(
            find_header_end(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"),
            Some(33)
        );
        assert_eq!(
            find_header_end(b"HTTP/1.1 200 OK\r\n\r\nbody"),
            Some(15)
        );
    }

    #[test]
    fn test_parse_status_code() {
        assert_eq!(parse_status_code("HTTP/1.1 206 Partial Content\r\n").unwrap(), 206);
        assert_eq!(parse_status_code("HTTP/1.1 200 OK\r\n").unwrap(), 200);
        assert!(parse_status_code("garbage").is_err());
        assert!(parse_status_code("").is_err());
    }

    #[test]
    fn test_is_chunked() {
        assert!(is_chunked("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"));
        assert!(is_chunked("HTTP/1.1 200 OK\r\ntransfer-encoding: CHUNKED\r\n\r\n"));
        assert!(!is_chunked("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n"));
    }

    #[test]
    fn test_parse_content_length() {
        assert_eq!(
            parse_content_length("HTTP/1.1 206\r\nContent-Length: 12345\r\n\r\n").unwrap(),
            Some(12345)
        );
        assert_eq!(
            parse_content_length("HTTP/1.1 206\r\ncontent-length: 0\r\n\r\n").unwrap(),
            Some(0)
        );
        assert_eq!(
            parse_content_length("HTTP/1.1 206\r\n\r\n").unwrap(),
            None
        );
    }

    #[test]
    fn test_write_all_at_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pwrite_test.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(100).unwrap();

        write_all_at_offset(&file, b"hello", 10).unwrap();
        write_all_at_offset(&file, b"world", 50).unwrap();

        drop(file);
        let content = std::fs::read(&path).unwrap();
        assert_eq!(&content[10..15], b"hello");
        assert_eq!(&content[50..55], b"world");
        assert_eq!(&content[0..10], &[0u8; 10]);
    }
}

// Non-Linux test: verify the stub returns Err(Unsupported).
#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_splice_unsupported_on_non_linux() {
        let file = tempfile::tempfile().unwrap();
        let result = try_splice_download("http://example.com/file", 0, 100, &file, 0).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }
}
