#![allow(dead_code)]
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::watch;

const SMALL_CONTENT: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
const MEDIUM_PATTERN: u8 = 0xAB;
const LARGE_PATTERN: u8 = 0xCD;

pub struct TestServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    slow_gap_attempts: Arc<AtomicUsize>,
    slow_gap_updates: watch::Sender<usize>,
    error_404_requests: Arc<AtomicUsize>,
    error_503_requests: Arc<AtomicUsize>,
    error_500_requests: Arc<AtomicUsize>,
}

impl TestServer {
    pub async fn start() -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr)
            .await
            .expect("Failed to bind test server port");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let slow_gap_attempts = Arc::new(AtomicUsize::new(0));
        let handler_slow_gap_attempts = Arc::clone(&slow_gap_attempts);
        let (slow_gap_updates, _) = watch::channel(0usize);
        let handler_slow_gap_updates = slow_gap_updates.clone();
        let slow_gap_fallback_started = Arc::new(AtomicBool::new(false));
        let handler_slow_gap_fallback_started = Arc::clone(&slow_gap_fallback_started);
        let error_404_requests = Arc::new(AtomicUsize::new(0));
        let handler_error_404_requests = Arc::clone(&error_404_requests);
        let error_503_requests = Arc::new(AtomicUsize::new(0));
        let handler_error_503_requests = Arc::clone(&error_503_requests);
        let error_500_requests = Arc::new(AtomicUsize::new(0));
        let handler_error_500_requests = Arc::clone(&error_500_requests);

        tokio::spawn(async move {
            // A stalled response must not prevent the listener from accepting
            // the other range requests in a concurrent-download test.
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                connections.spawn(Self::handle_connection(
                                    stream,
                                    Arc::clone(&handler_slow_gap_attempts),
                                    handler_slow_gap_updates.clone(),
                                    Arc::clone(&handler_slow_gap_fallback_started),
                                    Arc::clone(&handler_error_404_requests),
                                    Arc::clone(&handler_error_503_requests),
                                    Arc::clone(&handler_error_500_requests),
                                ));
                            }
                            Err(_) => break,
                        }
                    }
                    Some(_) = connections.join_next() => {}
                    _ = &mut shutdown_rx => break,
                }
            }

            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });

        TestServer {
            addr: actual_addr,
            shutdown: Some(shutdown_tx),
            slow_gap_attempts,
            slow_gap_updates,
            error_404_requests,
            error_503_requests,
            error_500_requests,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn slow_gap_attempts(&self) -> usize {
        self.slow_gap_attempts.load(Ordering::SeqCst)
    }

    pub async fn wait_for_slow_gap_attempt(&self, expected: usize) {
        let mut updates = self.slow_gap_updates.subscribe();
        loop {
            if *updates.borrow() >= expected {
                return;
            }
            updates
                .changed()
                .await
                .expect("test server gap progress sender must remain alive");
        }
    }

    pub fn error_404_requests(&self) -> usize {
        self.error_404_requests.load(Ordering::SeqCst)
    }

    pub fn error_503_requests(&self) -> usize {
        self.error_503_requests.load(Ordering::SeqCst)
    }

    pub fn error_500_requests(&self) -> usize {
        self.error_500_requests.load(Ordering::SeqCst)
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        const MAX_HEADER_SIZE: usize = 16 * 1024;
        let mut request = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];

        while request.len() < MAX_HEADER_SIZE {
            let n = stream.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }

            let remaining = MAX_HEADER_SIZE - request.len();
            request.extend_from_slice(&chunk[..n.min(remaining)]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        request
    }

    async fn handle_connection(
        mut stream: tokio::net::TcpStream,
        slow_gap_attempts: Arc<AtomicUsize>,
        slow_gap_updates: watch::Sender<usize>,
        slow_gap_fallback_started: Arc<AtomicBool>,
        error_404_requests: Arc<AtomicUsize>,
        error_503_requests: Arc<AtomicUsize>,
        error_500_requests: Arc<AtomicUsize>,
    ) {
        let request = Self::read_request(&mut stream).await;
        let request_str = String::from_utf8_lossy(&request);
        let first_line = request_str.lines().next().unwrap_or("");
        let mut parts = first_line.split(' ');
        parts.next();
        let path = parts.next().unwrap_or("");
        if path.starts_with("/files/timeout_")
            || path.starts_with("/files/disconnect_")
            || path == "/files/slow_stream_test.bin"
            || path == "/files/tiny_stream_test.bin"
            || path == "/files/slow_gap_test.bin"
        {
            let _ = Self::handle_async_request(
                &mut stream,
                &request,
                slow_gap_attempts,
                slow_gap_updates,
                slow_gap_fallback_started,
            )
            .await;
        } else {
            if path == "/error/404" || path == "/files/concurrent_404_test.bin" {
                error_404_requests.fetch_add(1, Ordering::SeqCst);
            } else if path == "/error/503" {
                error_503_requests.fetch_add(1, Ordering::SeqCst);
            } else if path == "/error/500" {
                error_500_requests.fetch_add(1, Ordering::SeqCst);
            }
            let response = Self::handle_request(&request);
            let _ = stream.write_all(&response).await;
            let _ = stream.flush().await;
        }
    }

    async fn handle_async_request(
        stream: &mut tokio::net::TcpStream,
        request: &[u8],
        slow_gap_attempts: Arc<AtomicUsize>,
        slow_gap_updates: watch::Sender<usize>,
        slow_gap_fallback_started: Arc<AtomicBool>,
    ) -> std::io::Result<()> {
        let request_str = String::from_utf8_lossy(request);
        let first_line = request_str.lines().next().unwrap_or("");
        let mut parts = first_line.split(' ');
        let path = parts.nth(1).unwrap_or("");

        match path {
            "/files/slow_gap_test.bin" => {
                const TOTAL: usize = 2 * 1024 * 1024;
                let range = request_str.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("range")
                        .then_some(value.trim().strip_prefix("bytes="))??
                        .split_once('-')
                        .map(|(start, end)| {
                            (start.parse::<usize>().ok(), end.parse::<usize>().ok())
                        })
                });

                let Some((Some(start), Some(end))) = range else {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {TOTAL}\r\n\r\n"
                    );
                    stream.write_all(header.as_bytes()).await?;
                    return stream.flush().await;
                };

                let end = end.min(TOTAL - 1);
                let stalled_attempt = if start >= TOTAL / 2 {
                    let attempt = slow_gap_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    if attempt == 1 {
                        slow_gap_fallback_started.store(true, Ordering::Release);
                        slow_gap_updates.send_replace(attempt);
                        stream
                            .write_all(
                                b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */2097152\r\nContent-Length: 0\r\n\r\n",
                            )
                            .await?;
                        return stream.flush().await;
                    }
                    Some(attempt)
                } else if slow_gap_fallback_started.load(Ordering::Acquire)
                    && start == 0
                    && end + 1 == TOTAL
                {
                    Some(slow_gap_attempts.fetch_add(1, Ordering::SeqCst) + 1)
                } else {
                    None
                };

                if let Some(attempt) = stalled_attempt {
                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Range: bytes {start}-{end}/{TOTAL}\r\nContent-Length: {}\r\n\r\n",
                        end - start + 1
                    );
                    stream.write_all(header.as_bytes()).await?;
                    stream.flush().await?;
                    if request_str.starts_with("HEAD ") {
                        return Ok(());
                    }

                    let chunk = vec![0x6b; 64 * 1024];
                    let mut remaining = end - start + 1;
                    let mut first_chunk = true;
                    while remaining > 0 {
                        let size = remaining.min(chunk.len());
                        stream.write_all(&chunk[..size]).await?;
                        stream.flush().await?;
                        remaining -= size;
                        if first_chunk {
                            // Signal only after the client can observe a
                            // stalled body, rather than when headers arrive.
                            slow_gap_updates.send_replace(attempt);
                            first_chunk = false;
                        }
                        if remaining > 0 {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    }
                    return Ok(());
                }

                let body = vec![0x6b; end - start + 1];
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Range: bytes {start}-{end}/{TOTAL}\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).await?;
                stream.write_all(&body).await?;
                stream.flush().await
            }
            "/files/slow_stream_test.bin" => {
                let header = b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 2097152\r\n\r\n";
                stream.write_all(header).await?;
                stream.flush().await?;
                if request_str.starts_with("HEAD ") {
                    return Ok(());
                }

                let chunk = vec![0x5a; 64 * 1024];
                for _ in 0..32 {
                    stream.write_all(&chunk).await?;
                    stream.flush().await?;
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Ok(())
            }
            "/files/tiny_stream_test.bin" => {
                const TOTAL: usize = 32 * 1024;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {TOTAL}\r\n\r\n"
                );
                stream.write_all(header.as_bytes()).await?;
                stream.flush().await?;
                if request_str.starts_with("HEAD ") {
                    return Ok(());
                }

                let chunk = vec![0x61; 512];
                for _ in 0..(TOTAL / chunk.len()) {
                    stream.write_all(&chunk).await?;
                    stream.flush().await?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Ok(())
            }
            "/files/timeout_test.bin" => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let body: Vec<u8> = (0..=100u8).collect();
                let response = http_response(200, "application/octet-stream", &body);
                stream.write_all(&response).await?;
                stream.flush().await
            }
            "/files/disconnect_test.bin" => {
                let partial_response = b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 1000\r\n\r\n";
                stream.write_all(partial_response).await?;
                stream.flush().await?;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Ok(())
            }
            "/files/disconnect_range_test.bin" => {
                let range_header = if request_str.contains("Range:") {
                    Some(
                        request_str
                            .split("Range: ")
                            .nth(1)
                            .and_then(|r| r.lines().next())
                            .unwrap_or(""),
                    )
                } else {
                    None
                };

                if let Some(range) = range_header
                    && let Some((start_str, _)) = range
                        .trim()
                        .strip_prefix("bytes=")
                        .and_then(|r| r.split_once('-'))
                {
                    let start: usize = start_str.parse().unwrap_or(0);
                    if (100..200).contains(&start) {
                        let partial_response = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes=100-149/250\r\nContent-Length: 50\r\n\r\n";
                        stream.write_all(partial_response).await?;
                        stream.flush().await?;
                        let partial_body: Vec<u8> = (100..125).map(|i| i as u8).collect();
                        stream.write_all(&partial_body).await?;
                        stream.flush().await?;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        return Ok(());
                    }
                }

                let body: Vec<u8> = (0..=250u8).collect();
                let response = http_response(200, "application/octet-stream", &body);
                stream.write_all(&response).await?;
                stream.flush().await
            }
            _ => {
                let response = Self::handle_request(request);
                stream.write_all(&response).await?;
                stream.flush().await
            }
        }
    }

    fn handle_request(request: &[u8]) -> Vec<u8> {
        let request_str = String::from_utf8_lossy(request);
        let first_line = request_str.lines().next().unwrap_or("");
        let mut parts = first_line.split(' ');
        let method = parts.next().unwrap_or("GET");
        let path = parts.next().unwrap_or("/");
        let is_head = method.eq_ignore_ascii_case("HEAD");

        let response = match path {
            "/files/small.bin" => {
                let body = SMALL_CONTENT;
                http_response(200, "application/octet-stream", body)
            }
            "/files/no-range.bin" => {
                // Deliberately ignores Range headers. This models a server
                // which advertises a stable entity but cannot resume a
                // partially downloaded file.
                let body = b"resume-me";
                http_response(200, "application/octet-stream", body)
            }
            "/files/resume-range.bin" => {
                // Mirror used by resume failover tests. Unlike no-range.bin,
                // this endpoint honors a byte range against the same entity.
                let body = b"resume-me";
                let range_header = request_str
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("range").then_some(value.trim())
                    });
                if let Some(range) = range_header
                    && let Some(start) = range
                        .strip_prefix("bytes=")
                        .and_then(|value| value.split('-').next())
                        .and_then(|value| value.parse::<usize>().ok())
                    && start < body.len()
                {
                    let partial = &body[start..];
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes={}-{}/{}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
                        start,
                        body.len() - 1,
                        body.len(),
                        partial.len()
                    )
                    .into_bytes()
                    .into_iter()
                    .chain(partial.iter().copied())
                    .collect()
                } else {
                    http_response(200, "application/octet-stream", body)
                }
            }
            "/files/medium.bin" => {
                let body = vec![MEDIUM_PATTERN; 1024 * 1024];
                http_response(200, "application/octet-stream", &body)
            }
            "/files/large.bin" => {
                let body = vec![LARGE_PATTERN; 10 * 1024 * 1024];
                http_response(200, "application/octet-stream", &body)
            }
            "/files/concurrent_404_test.bin" => {
                if is_head {
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 2000000\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n".to_vec()
                } else {
                    http_404()
                }
            }
            "/files/range_test.bin" => {
                let range_header = if request_str.contains("Range:") {
                    Some(request_str.split("Range: ").nth(1).and_then(|r| r.lines().next()).unwrap_or(""))
                } else { None };

                if let Some(range) = range_header {
                    if let Some((start_str, end_str)) = range.trim().strip_prefix("bytes=").and_then(|r| r.split_once('-')) {
                        let start: usize = start_str.parse().unwrap_or(0);
                        let end: usize = end_str.parse().unwrap_or(99);
                        let total = 100u8;
                        let body: Vec<u8> = (start..=end.min(total as usize)).map(|i| i as u8).collect();
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes={}-{}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
                            start, end.min(total as usize), body.len()
                        ).into_bytes()
                        .into_iter().chain(body).collect()
                    } else { http_404() }
                } else {
                    let body: Vec<u8> = (0..=100u8).collect();
                    http_response(200, "application/octet-stream", &body)
                }
            }
            "/redirect" => {
                b"HTTP/1.1 302 Found\r\nLocation: /files/small.bin\r\nContent-Length: 0\r\n\r\n".to_vec()
            }
            "/redirect_missing" => {
                b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n".to_vec()
            }
            "/slow" => {
                b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()
            }
            "/error/500" => {
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_vec()
            }
            "/error/503" => {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec()
            }
            "/error/404" => {
                http_404()
            }
            "/chunked" => {
                let body = b"Hello, chunked world!";
                let header = format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                    body.len(),
                    String::from_utf8_lossy(body)
                );
                header.into_bytes()
            }
            "/files/retry_test.bin" => {
                let range_header = if request_str.contains("Range:") {
                    Some(request_str.split("Range: ").nth(1).and_then(|r| r.lines().next()).unwrap_or(""))
                } else { None };

                if let Some(range) = range_header {
                    if let Some((start_str, end_str)) = range.trim().strip_prefix("bytes=").and_then(|r| r.split_once('-')) {
                        let start: usize = start_str.parse().unwrap_or(0);
                        let end: usize = end_str.parse().unwrap_or(99);
                        let total = 200u8;

                        if start >= 100 && request_str.contains("fail_on_second") {
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_vec()
                        } else {
                            let body: Vec<u8> = (start..=end.min(total as usize)).map(|i| i as u8).collect();
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes={}-{}/{}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
                                start, end.min(total as usize), total, body.len()
                            ).into_bytes()
                            .into_iter().chain(body).collect()
                        }
                    } else { http_404() }
                } else {
                    let body: Vec<u8> = (0..=200u8).collect();
                    http_response(200, "application/octet-stream", &body)
                }
            }
            "/files/partial_fail_test.bin" => {
                let range_header = if request_str.contains("Range:") {
                    Some(request_str.split("Range: ").nth(1).and_then(|r| r.lines().next()).unwrap_or(""))
                } else { None };

                if let Some(range) = range_header {
                    if let Some((start_str, end_str)) = range.trim().strip_prefix("bytes=").and_then(|r| r.split_once('-')) {
                        let start: usize = start_str.parse().unwrap_or(0);
                        let end: usize = end_str.parse().unwrap_or(99);
                        let total = 250u8;

                        if (100..200).contains(&start) {
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_vec()
                        } else {
                            let body: Vec<u8> = (start..=end.min(total as usize)).map(|i| i as u8).collect();
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes={}-{}/{}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
                                start, end.min(total as usize), total, body.len()
                            ).into_bytes()
                            .into_iter().chain(body).collect()
                        }
                    } else { http_404() }
                } else {
                    let body: Vec<u8> = (0..=250u8).collect();
                    http_response(200, "application/octet-stream", &body)
                }
            }
            "/files/concurrent_416_test.bin" => {
                let range_header = if request_str.contains("Range:") {
                    Some(request_str.split("Range: ").nth(1).and_then(|r| r.lines().next()).unwrap_or(""))
                } else { None };

                if let Some(range) = range_header {
                    if let Some((start_str, end_str)) = range.trim().strip_prefix("bytes=").and_then(|r| r.split_once('-')) {
                        let start: usize = start_str.parse().unwrap_or(0);
                        let end: usize = end_str.parse().unwrap_or(99);
                        let total = 2000000u64;

                        if (500000..1000000).contains(&start) {
                            format!(
                                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\n\r\n",
                                total
                            ).into_bytes()
                        } else {
                            let actual_end = std::cmp::min(end, (total - 1) as usize);
                            let body: Vec<u8> = (start..=actual_end).map(|i| (i % 256) as u8).collect();
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes={}-{}/{}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
                                start, actual_end, total, body.len()
                            ).into_bytes()
                            .into_iter().chain(body).collect()
                        }
                    } else { http_404() }
                } else {
                    let body: Vec<u8> = (0..2000000).map(|i| (i % 256) as u8).collect();
                    http_response(200, "application/octet-stream", &body)
                }
            }
            "/files/concurrent_server_error.bin" => {
                let range_header = if request_str.contains("Range:") {
                    Some(request_str.split("Range: ").nth(1).and_then(|r| r.lines().next()).unwrap_or(""))
                } else { None };

                if let Some(range) = range_header {
                    if let Some((start_str, end_str)) = range.trim().strip_prefix("bytes=").and_then(|r| r.split_once('-')) {
                        let start: usize = start_str.parse().unwrap_or(0);
                        let end: usize = end_str.parse().unwrap_or(99);
                        let total = 2000000u64;

                        if (500000..1000000).contains(&start) {
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_vec()
                        } else {
                            let actual_end = std::cmp::min(end, (total - 1) as usize);
                            let body: Vec<u8> = (start..=actual_end).map(|i| (i % 256) as u8).collect();
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes={}-{}/{}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
                                start, actual_end, total, body.len()
                            ).into_bytes()
                            .into_iter().chain(body).collect()
                        }
                    } else { http_404() }
                } else {
                    let body: Vec<u8> = (0..2000000).map(|i| (i % 256) as u8).collect();
                    http_response(200, "application/octet-stream", &body)
                }
            }
            _ => http_404(),
        };

        // For HEAD requests, return only the headers (up to and including the
        // blank-line separator). The Content-Length header is preserved so the
        // client knows the resource size, but no body bytes are sent. This
        // prevents the server from blocking on write_all when the client
        // (e.g. reqwest HEAD probe) closes the connection after reading headers.
        if is_head && let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") {
            return response[..pos + 4].to_vec();
        }

        response
    }
}

fn http_response(code: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        code,
        content_type,
        body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(body.to_vec())
    .collect()
}

fn http_404() -> Vec<u8> {
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub fn small_content() -> &'static [u8] {
    SMALL_CONTENT
}
pub fn medium_pattern() -> u8 {
    MEDIUM_PATTERN
}
pub fn large_pattern() -> u8 {
    LARGE_PATTERN
}
pub fn small_sha256() -> &'static str {
    "9a5f529b616b7a64c8b0bf3a46d9d6e3e088ce9a98a2aeb3e7b3d6b1c3d4e5f6"
}
