use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const SMALL_CONTENT: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
const MEDIUM_PATTERN: u8 = 0xAB;
const LARGE_PATTERN: u8 = 0xCD;

pub struct TestServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServer {
    pub async fn start() -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr)
            .await
            .expect("绑定测试服务器端口失败");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((mut stream, _)) => {
                                let request = Self::read_request(&mut stream).await;
                                let response = Self::handle_request(&request);
                                let _ = stream.write_all(&response).await;
                                let _ = stream.flush().await;
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        TestServer {
            addr: actual_addr,
            shutdown: Some(shutdown_tx),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        buf.truncate(n);
        buf
    }

    fn handle_request(request: &[u8]) -> Vec<u8> {
        let request_str = String::from_utf8_lossy(request);
        let path = request_str
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .unwrap_or("/");

        match path {
            "/files/small.bin" => {
                let body = SMALL_CONTENT;
                http_response(200, "application/octet-stream", body)
            }
            "/files/medium.bin" => {
                let body = vec![MEDIUM_PATTERN; 1024 * 1024];
                http_response(200, "application/octet-stream", &body)
            }
            "/files/large.bin" => {
                let body = vec![LARGE_PATTERN; 10 * 1024 * 1024];
                http_response(200, "application/octet-stream", &body)
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
                        .into_iter().chain(body.into_iter()).collect()
                    } else { http_404() }
                } else {
                    let body: Vec<u8> = (0..=100u8).map(|i| i).collect();
                    http_response(200, "application/octet-stream", &body)
                }
            }
            "/redirect" => {
                b"HTTP/1.1 302 Found\r\nLocation: /files/small.bin\r\nContent-Length: 0\r\n\r\n".to_vec()
            }
            "/slow" => {
                b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()
            }
            "/error/500" => {
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_vec()
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
            _ => http_404(),
        }
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
    .chain(body.to_vec().into_iter())
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
