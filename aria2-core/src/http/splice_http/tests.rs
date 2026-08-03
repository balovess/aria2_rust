//! Integration and unit tests for the splice HTTP download module.
//!
//! Linux-only tests exercise the full splice download pipeline via a mock
//! HTTP server. Non-Linux tests verify the unsupported-platform stub.

// =========================================================================
// Linux tests
// =========================================================================

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::super::download::try_splice_download;
    use super::super::helpers::{
        find_header_end, is_chunked, parse_content_length, parse_status_code, write_all_at_offset,
    };
    use std::io;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Spawn a mock HTTP server that responds to a Range request with 206
    /// Partial Content. Returns the server address.
    ///
    /// The server sends `body[offset..offset+length]` as the response body
    /// with a Content-Length header.
    async fn spawn_mock_206_server(body: Vec<u8>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("get mock server local addr");

        tokio::spawn(async move {
            let (mut sock, _) = listener
                .accept()
                .await
                .expect("mock server accept connection");

            // Read the request (until \r\n\r\n).
            let mut req_buf = [0u8; 4096];
            let _n = sock
                .read(&mut req_buf)
                .await
                .expect("mock server read request");

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

            sock.write_all(response.as_bytes())
                .await
                .expect("mock server write headers");
            sock.write_all(chunk).await.expect("mock server write body");
        });

        addr
    }

    /// Parse `Range: bytes=START-END` from a raw HTTP request string.
    fn parse_request_range(req: &str, total: usize) -> (usize, usize) {
        for line in req.lines() {
            if let Some(rest) = line.strip_prefix("Range:") {
                let rest = rest.trim();
                if let Some(range) = rest.strip_prefix("bytes=")
                    && let Some((start_s, end_s)) = range.split_once('-')
                {
                    let start: usize = start_s.parse().unwrap_or(0);
                    let end: usize = end_s.parse().unwrap_or(total - 1);
                    let length = end.saturating_sub(start) + 1;
                    return (start, length);
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
        let dir = tempfile::tempdir().expect("create temp dir");
        let out_path = dir.path().join("out.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .expect("create output file");

        let url = format!("http://{addr}/test.bin");
        let n = try_splice_download(&url, 0, payload.len() as u64, &file, 0)
            .await
            .expect("splice download should succeed");

        assert_eq!(n, payload.len() as u64);

        // Verify the file content matches the payload.
        drop(file);
        let content = std::fs::read(&out_path).expect("read output file");
        assert_eq!(content, payload);
    }

    #[tokio::test]
    async fn test_splice_download_range_offset() {
        // Download a sub-range starting at a non-zero offset.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
        let addr = spawn_mock_206_server(payload.clone()).await;

        let dir = tempfile::tempdir().expect("create temp dir");
        let out_path = dir.path().join("out_range.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .expect("create output file");

        let offset = 50_000u64;
        let length = 80_000u64;
        let url = format!("http://{addr}/test.bin");
        let n = try_splice_download(&url, offset, length, &file, 0)
            .await
            .expect("splice range download should succeed");

        assert_eq!(n, length);

        // Verify the file content matches the requested sub-range.
        drop(file);
        let content = std::fs::read(&out_path).expect("read output file");
        assert_eq!(content.len(), length as usize);
        assert_eq!(
            content,
            &payload[offset as usize..(offset + length) as usize]
        );
    }

    #[tokio::test]
    async fn test_splice_download_with_file_offset() {
        // Splice data at a non-zero file offset (concurrent segment scenario).
        let payload: Vec<u8> = (0..64_000u32).map(|i| (i % 256) as u8).collect();
        let addr = spawn_mock_206_server(payload.clone()).await;

        let dir = tempfile::tempdir().expect("create temp dir");
        let out_path = dir.path().join("out_foff.bin");
        // Pre-allocate a larger file so pwrite at offset works.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .expect("create output file");
        file.set_len(100_000).expect("pre-allocate file");

        let file_offset = 30_000u64;
        let url = format!("http://{addr}/test.bin");
        let n = try_splice_download(&url, 0, payload.len() as u64, &file, file_offset)
            .await
            .expect("splice with file offset should succeed");

        assert_eq!(n, payload.len() as u64);

        // Verify the data was written at the correct offset.
        drop(file);
        let content = std::fs::read(&out_path).expect("read output file");
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
        let file = tempfile::tempfile().expect("create temp file");
        let result = try_splice_download("https://example.com/file", 0, 100, &file, 0).await;
        assert!(result.is_err());
        let err = result.expect_err("https should be rejected");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn test_splice_download_zero_length() {
        let file = tempfile::tempfile().expect("create temp file");
        let result = try_splice_download("http://example.com/file", 0, 0, &file, 0).await;
        assert!(result.is_ok());
        assert_eq!(result.expect("zero-length should return Ok"), 0);
    }

    #[tokio::test]
    async fn test_splice_download_non_206_falls_back() {
        // Server returns 200 (not 206) — splice should return Err so the
        // caller falls back to reqwest.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("get test server local addr");

        tokio::spawn(async move {
            let (mut sock, _) = listener
                .accept()
                .await
                .expect("test server accept connection");
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await.expect("test server read request");
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            )
            .await
            .expect("test server write 200 response");
        });

        let dir = tempfile::tempdir().expect("create temp dir");
        let out_path = dir.path().join("out_200.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .expect("create output file");

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

        let dir = tempfile::tempdir().expect("create temp dir");
        let out_path = dir.path().join("out_tiny.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .expect("create output file");

        let url = format!("http://{addr}/test.bin");
        let n = try_splice_download(&url, 0, payload.len() as u64, &file, 0)
            .await
            .expect("splice tiny download should succeed");

        assert_eq!(n, payload.len() as u64);

        drop(file);
        let content = std::fs::read(&out_path).expect("read output file");
        assert_eq!(content, payload);
    }

    #[test]
    fn test_find_header_end() {
        assert_eq!(find_header_end(b""), None);
        assert_eq!(find_header_end(b"HTTP/1.1 200 OK\r\n"), None);
        assert_eq!(
            find_header_end(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"),
            Some(34)
        );
        assert_eq!(find_header_end(b"HTTP/1.1 200 OK\r\n\r\nbody"), Some(15));
    }

    #[test]
    fn test_parse_status_code() {
        assert_eq!(
            parse_status_code("HTTP/1.1 206 Partial Content\r\n").expect("parse 206"),
            206
        );
        assert_eq!(
            parse_status_code("HTTP/1.1 200 OK\r\n").expect("parse 200"),
            200
        );
        assert!(parse_status_code("garbage").is_err());
        assert!(parse_status_code("").is_err());
    }

    #[test]
    fn test_is_chunked() {
        assert!(is_chunked(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
        ));
        assert!(is_chunked(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: CHUNKED\r\n\r\n"
        ));
        assert!(!is_chunked(
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n"
        ));
    }

    #[test]
    fn test_parse_content_length() {
        assert_eq!(
            parse_content_length("HTTP/1.1 206\r\nContent-Length: 12345\r\n\r\n")
                .expect("parse content-length 12345"),
            Some(12345)
        );
        assert_eq!(
            parse_content_length("HTTP/1.1 206\r\ncontent-length: 0\r\n\r\n")
                .expect("parse content-length 0"),
            Some(0)
        );
        assert_eq!(
            parse_content_length("HTTP/1.1 206\r\n\r\n").expect("parse missing content-length"),
            None
        );
    }

    #[test]
    fn test_write_all_at_offset() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("pwrite_test.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("create pwrite test file");
        file.set_len(100).expect("pre-allocate pwrite test file");

        write_all_at_offset(&file, b"hello", 10).expect("pwrite hello at offset 10");
        write_all_at_offset(&file, b"world", 50).expect("pwrite world at offset 50");

        drop(file);
        let content = std::fs::read(&path).expect("read pwrite test file");
        assert_eq!(&content[10..15], b"hello");
        assert_eq!(&content[50..55], b"world");
        assert_eq!(&content[0..10], &[0u8; 10]);
    }
}

// =========================================================================
// Non-Linux tests
// =========================================================================

/// Non-Linux test: verify the stub returns Err(Unsupported).
#[cfg(all(test, not(target_os = "linux")))]
mod non_linux_tests {
    use super::super::download::try_splice_download;
    use std::io;

    #[tokio::test]
    async fn test_splice_unsupported_on_non_linux() {
        let file = tempfile::tempfile().expect("create temp file");
        let result = try_splice_download("http://example.com/file", 0, 100, &file, 0).await;
        assert!(result.is_err());
        assert_eq!(
            result.expect_err("non-linux should return Err").kind(),
            io::ErrorKind::Unsupported
        );
    }
}
