//! HTTP proxy tunneling module
//!
//! Implements HTTP-over-HTTP-proxy connections, supporting both CONNECT
//! tunneling (for HTTPS/FTP) and forward proxy mode (absolute URI for
//! plain HTTP). Collapses the C++ command chain:
//!
//! - `AbstractProxyRequestCommand` (sends proxy request)
//! - `AbstractProxyResponseCommand` (validates proxy response, non-200 = error)
//! - `HttpProxyRequestCommand` (builds CONNECT or forward request)
//! - `HttpProxyResponseCommand` (reads response, proceeds to next command)
//!
//! into a single async function using Rust's async/await model.
//!
//! # Proxy Modes
//!
//! - **Tunnel**: `CONNECT host:port HTTP/1.1` — transparent TCP tunnel
//!   for HTTPS and FTP-over-proxy connections.
//! - **Forward**: Absolute-URI forwarding — `GET http://target/path HTTP/1.1`
//!   for plain HTTP through a proxy.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::digest_auth::DigestAuthChallenge;
use crate::http::header_processor::HttpHeaderProcessor;
use crate::http::request_response::basic_auth;

// ---------------------------------------------------------------------------
// HttpProxyType
// ---------------------------------------------------------------------------

/// The type of proxy connection that was established.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HttpProxyType {
    /// CONNECT tunnel -- the proxy relays raw bytes (HTTPS, FTP).
    Tunnel,
    /// Forward -- the proxy sees absolute-URI requests (plain HTTP).
    Forward,
}

// ---------------------------------------------------------------------------
// HttpProxyTunnelConfig
// ---------------------------------------------------------------------------

/// Configuration for establishing an HTTP proxy tunnel or forward connection.
#[derive(Debug, Clone)]
pub struct HttpProxyTunnelConfig {
    /// Proxy server hostname
    pub proxy_host: String,
    /// Proxy server port
    pub proxy_port: u16,
    /// Target server hostname
    pub target_host: String,
    /// Target server port
    pub target_port: u16,
    /// Proxy authentication username (None = no auth)
    pub username: Option<String>,
    /// Proxy authentication password
    pub password: Option<String>,
    /// Connection timeout for connecting to the proxy
    pub connect_timeout: Duration,
    /// Read timeout for proxy response
    pub read_timeout: Duration,
    /// Write timeout for sending requests
    pub write_timeout: Duration,
    /// User-Agent header value
    pub user_agent: String,
}

impl Default for HttpProxyTunnelConfig {
    fn default() -> Self {
        Self {
            proxy_host: String::new(),
            proxy_port: 8080,
            target_host: String::new(),
            target_port: 80,
            username: None,
            password: None,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(30),
            user_agent: "aria2/rust".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// HttpProxyTunnelResult
// ---------------------------------------------------------------------------

/// Result of successfully establishing a proxy connection.
#[derive(Debug)]
pub struct HttpProxyTunnelResult {
    /// The TCP stream (tunneled or connected through the proxy)
    pub stream: TcpStream,
    /// The type of proxy connection established
    pub proxy_type: HttpProxyType,
}

// ---------------------------------------------------------------------------
// Top-level entry point
// ---------------------------------------------------------------------------

/// Establish an HTTP proxy connection (tunnel or forward).
///
/// This is the primary entry point, equivalent to the C++ command chain:
/// `AbstractProxyRequestCommand` + `AbstractProxyResponseCommand` +
/// `HttpProxyRequestCommand` + `HttpProxyResponseCommand`.
///
/// - For tunnel mode, sends `CONNECT host:port HTTP/1.1` and validates the
///   proxy response. On success the `TcpStream` is a transparent tunnel.
/// - For forward mode, connects to the proxy and returns the `TcpStream`
///   for the caller to send the absolute-URI request through.
/// - Handles 407 Proxy Authentication Required with Basic/Digest retry.
///
/// # Errors
///
/// - `RecoverableError::Timeout` on connection or read timeout
/// - `RecoverableError::TemporaryNetworkFailure` on proxy rejection,
///   authentication failure, or I/O errors
pub async fn establish_http_proxy_tunnel(
    config: &HttpProxyTunnelConfig,
    proxy_type: HttpProxyType,
) -> Result<HttpProxyTunnelResult> {
    match proxy_type {
        HttpProxyType::Tunnel => {
            let stream = HttpProxyTunnel::establish_tunnel(config).await?;
            Ok(HttpProxyTunnelResult { stream, proxy_type: HttpProxyType::Tunnel })
        }
        HttpProxyType::Forward => {
            let stream = HttpProxyTunnel::establish_forward(config).await?;
            Ok(HttpProxyTunnelResult { stream, proxy_type: HttpProxyType::Forward })
        }
    }
}

// ---------------------------------------------------------------------------
// HttpProxyTunnel -- internal implementation
// ---------------------------------------------------------------------------

/// HTTP proxy tunnel handler.
struct HttpProxyTunnel;

impl HttpProxyTunnel {
    const MAX_AUTH_RETRIES: u32 = 1;

    async fn establish_tunnel(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        let stream = Self::connect_to_proxy(config).await?;
        let stream = Self::tunnel_handshake(stream, config, 0).await?;
        info!("HTTP proxy tunnel established: {}:{} -> {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port);
        Ok(stream)
    }

    async fn establish_forward(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        let stream = Self::connect_to_proxy(config).await?;
        info!("HTTP forward-proxy connection: {}:{} for target {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port);
        Ok(stream)
    }

    // -- Connection & handshake ------------------------------------------------

    async fn connect_to_proxy(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        debug!("Connecting to proxy {}:{} for {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port);
        let stream = timeout(
            config.connect_timeout,
            TcpStream::connect((config.proxy_host.as_str(), config.proxy_port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Failed to connect to proxy {}:{}: {}", config.proxy_host, config.proxy_port, e),
        }))?;
        stream.set_nodelay(true).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {}", e),
            })
        })?;
        Ok(stream)
    }

    /// Perform the CONNECT handshake with proxy auth retry logic.
    ///
    /// Uses a loop instead of recursion to avoid `Pin<Box<dyn Future>>`
    /// lifetime issues. Max auth retries bounded by `MAX_AUTH_RETRIES`.
    async fn tunnel_handshake(
        mut stream: TcpStream, config: &HttpProxyTunnelConfig, _auth_retry: u32,
    ) -> Result<TcpStream> {
        let auth_header = Self::maybe_preemptive_basic_auth(config);
        let request = Self::build_connect_request(config, auth_header.as_deref());
        Self::send_request(&mut stream, &request, config.write_timeout).await?;

        let mut remaining_retries = Self::MAX_AUTH_RETRIES;
        loop {
            let response = Self::read_proxy_response(&mut stream, config.read_timeout).await?;
            match response.status_code {
                200 => return Ok(stream),
                407 if remaining_retries > 0 => {
                    remaining_retries -= 1;
                    let has_creds = config.username.as_ref().is_some_and(|u| !u.is_empty());
                    if !has_creds {
                        return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            message: format!("Proxy {}:{} requires auth but no credentials provided",
                                config.proxy_host, config.proxy_port),
                        }));
                    }
                    Self::consume_response_body(&mut stream, config.read_timeout).await?;
                    let proxy_authenticate = response.headers.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("proxy-authenticate"))
                        .map(|(_, v)| v.as_str()).unwrap_or("");
                    let username = config.username.as_deref().unwrap_or("");
                    let password = config.password.as_deref().unwrap_or("");
                    let uri = format!("{}:{}", config.target_host, config.target_port);
                    let auth_header = if proxy_authenticate.starts_with("Digest") {
                        Self::build_digest_auth_header(username, password, proxy_authenticate, &uri)
                    } else if proxy_authenticate.starts_with("Basic") {
                        basic_auth(username, password)
                    } else {
                        warn!("Unsupported proxy auth scheme: {}", proxy_authenticate);
                        return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            message: format!("Unsupported proxy auth: {}", proxy_authenticate),
                        }));
                    };
                    let request = Self::build_connect_request(config, Some(&auth_header));
                    Self::send_request(&mut stream, &request, config.write_timeout).await?;
                    // Loop back to read the response to the auth'd request
                }
                407 => {
                    return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("Proxy auth failed for {}:{}", config.proxy_host, config.proxy_port),
                    }));
                }
                status => {
                    return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("Proxy {}:{} rejected CONNECT to {}:{}: {} {}",
                            config.proxy_host, config.proxy_port,
                            config.target_host, config.target_port, status, response.reason_phrase),
                    }));
                }
            }
        }
    }

    // -- Request building ------------------------------------------------------

    /// Build the CONNECT request string (matches C++ `HttpRequest::createProxyRequest`).
    pub fn build_connect_request(config: &HttpProxyTunnelConfig, auth_header: Option<&str>) -> String {
        let mut request = format!(
            "CONNECT {}:{} HTTP/1.1\r\nUser-Agent: {}\r\nHost: {}:{}\r\n",
            config.target_host, config.target_port, config.user_agent,
            config.target_host, config.target_port
        );
        if let Some(auth) = auth_header {
            request.push_str(&format!("Proxy-Authorization: {}\r\n", auth));
        }
        request.push_str("\r\n");
        request
    }

    /// Build an absolute-URI forward proxy request line.
    pub fn build_forward_request_line(method: &str, target_url: &str) -> String {
        format!("{} {} HTTP/1.1\r\n", method, target_url)
    }

    /// Return pre-emptive Basic auth header if credentials are configured.
    fn maybe_preemptive_basic_auth(config: &HttpProxyTunnelConfig) -> Option<String> {
        let username = config.username.as_deref()?;
        if username.is_empty() { return None; }
        Some(basic_auth(username, config.password.as_deref().unwrap_or("")))
    }

    /// Build a Digest Proxy-Authorization header value.
    fn build_digest_auth_header(username: &str, password: &str, challenge_header: &str, uri: &str) -> String {
        let challenge = match DigestAuthChallenge::parse(challenge_header) {
            Ok(c) => c,
            Err(e) => {
                debug!("Failed to parse Digest challenge, fallback to Basic: {}", e);
                return basic_auth(username, password);
            }
        };
        let ha1 = md5_hex(&format!("{}:{}:{}", username, challenge.realm, password));
        let ha1 = if challenge.algorithm.eq_ignore_ascii_case("MD5-sess") {
            md5_hex(&format!("{}:{}:{}", ha1, challenge.nonce, "00000001"))
        } else { ha1 };
        let ha2 = md5_hex(&format!("CONNECT:{}", uri));
        let qop_value = challenge.qop.as_deref().unwrap_or("");
        let cnonce = "aria2rustcnonce";
        let response_hash = if qop_value.is_empty() {
            md5_hex(&format!("{}:{}:{}", ha1, challenge.nonce, ha2))
        } else {
            md5_hex(&format!("{}:{}:00000001:{}:{}:{}", ha1, challenge.nonce, cnonce, qop_value, ha2))
        };
        let mut header = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
            username, challenge.realm, challenge.nonce, uri, response_hash
        );
        if let Some(ref opaque) = challenge.opaque {
            header.push_str(&format!(", opaque=\"{}\"", opaque));
        }
        if !qop_value.is_empty() {
            header.push_str(&format!(", qop={}, nc=00000001, cnonce=\"{}\"", qop_value, cnonce));
        }
        header.push_str(&format!(", algorithm={}", challenge.algorithm));
        header
    }

    // -- I/O helpers -----------------------------------------------------------

    async fn send_request(stream: &mut TcpStream, request: &str, write_timeout: Duration) -> Result<()> {
        debug!("Sending proxy request:\n{}", request.trim_end());
        timeout(write_timeout, stream.write_all(request.as_bytes()))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to send proxy request: {}", e),
            }))?;
        stream.flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to flush proxy request: {}", e),
            })
        })
    }

    /// Read proxy response using streaming `HttpHeaderProcessor`.
    /// Skips 1xx informational responses (matching C++ behavior).
    async fn read_proxy_response(stream: &mut TcpStream, read_timeout: Duration) -> Result<ProxyResponse> {
        loop {
            let response = timeout(read_timeout, Self::parse_with_processor(stream))
                .await
                .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))??;
            if (100..200).contains(&response.status_code) {
                debug!("Skipping 1xx informational: {}", response.status_code);
                continue;
            }
            return Ok(response);
        }
    }

    async fn parse_with_processor(stream: &mut TcpStream) -> Result<ProxyResponse> {
        let mut processor = HttpHeaderProcessor::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to read proxy response: {}", e),
                })
            })?;
            if n == 0 {
                return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: "Connection closed while reading proxy response".to_string(),
                }));
            }
            let state = processor.feed(&buf[..n]);
            if state.is_complete() { break; }
            if state.is_error() {
                return Err(Aria2Error::Parse("Failed to parse proxy response".to_string()));
            }
        }
        let head = processor.get_result()?;
        let headers: Vec<(String, String)> = head.iter_headers()
            .map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Ok(ProxyResponse { status_code: head.status_code, reason_phrase: head.reason_phrase, headers })
    }

    /// Consume any remaining response body (for 407 responses).
    async fn consume_response_body(stream: &mut TcpStream, _read_timeout: Duration) -> Result<()> {
        let mut buf = [0u8; 4096];
        match timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
            Ok(Ok(n)) => { debug!("Consumed {} bytes of proxy response body", n); Ok(()) }
            Ok(Err(e)) => { debug!("Error consuming proxy response body: {}", e); Ok(()) }
            Err(_) => { debug!("Timeout consuming proxy response body (acceptable)"); Ok(()) }
        }
    }
}

// ---------------------------------------------------------------------------
// ProxyResponse (internal)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ProxyResponse {
    status_code: u16,
    reason_phrase: String,
    headers: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// MD5 helper
// ---------------------------------------------------------------------------

fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> HttpProxyTunnelConfig {
        HttpProxyTunnelConfig {
            proxy_host: "proxy.example.com".to_string(),
            proxy_port: 8080,
            username: None,
            password: None,
            target_host: "target.example.com".to_string(),
            target_port: 443,
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            user_agent: "aria2/rust".to_string(),
        }
    }

    #[test]
    fn test_connect_request_without_auth() {
        let config = test_config();
        let request = HttpProxyTunnel::build_connect_request(&config, None);
        assert!(request.starts_with("CONNECT target.example.com:443 HTTP/1.1\r\n"));
        assert!(request.contains("User-Agent: aria2/rust\r\n"));
        assert!(request.contains("Host: target.example.com:443\r\n"));
        assert!(!request.contains("Proxy-Authorization"));
        assert!(request.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_connect_request_with_basic_auth() {
        let config = test_config();
        let auth = basic_auth("user", "pass");
        let request = HttpProxyTunnel::build_connect_request(&config, Some(&auth));
        assert!(request.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
    }

    #[test]
    fn test_connect_request_format() {
        let config = test_config();
        let request = HttpProxyTunnel::build_connect_request(&config, None);
        let lines: Vec<&str> = request.trim_end().split("\r\n").collect();
        assert!(lines[0].starts_with("CONNECT "));
        assert!(lines[0].ends_with(" HTTP/1.1"));
    }

    #[test]
    fn test_forward_request_line_get() {
        assert_eq!(HttpProxyTunnel::build_forward_request_line("GET", "http://t.com/p"),
            "GET http://t.com/p HTTP/1.1\r\n");
    }

    #[test]
    fn test_basic_auth_header() {
        assert_eq!(basic_auth("user", "pass"), "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn test_digest_auth_header() {
        let challenge = r#"Digest realm="test@ex.com", nonce="abc", qop="auth", algorithm=MD5"#;
        let header = HttpProxyTunnel::build_digest_auth_header("user", "pass", challenge, "t:443");
        assert!(header.starts_with("Digest"));
        assert!(header.contains("username=\"user\""));
        assert!(header.contains("realm=\"test@ex.com\""));
        assert!(header.contains("response=\""));
    }

    #[test]
    fn test_digest_auth_fallback_on_bad_challenge() {
        let header = HttpProxyTunnel::build_digest_auth_header("u", "p", "NotDigest", "t:443");
        assert!(header.starts_with("Basic "));
    }

    #[test]
    fn test_preemptive_auth_with_credentials() {
        let config = HttpProxyTunnelConfig { username: Some("u".into()), password: Some("p".into()), ..test_config() };
        assert!(HttpProxyTunnel::maybe_preemptive_basic_auth(&config).is_some());
    }

    #[test]
    fn test_preemptive_auth_without_credentials() {
        assert!(HttpProxyTunnel::maybe_preemptive_basic_auth(&test_config()).is_none());
    }

    #[test]
    fn test_preemptive_auth_empty_username() {
        let config = HttpProxyTunnelConfig { username: Some(String::new()), password: Some("p".into()), ..test_config() };
        assert!(HttpProxyTunnel::maybe_preemptive_basic_auth(&config).is_none());
    }

    #[test]
    fn test_parse_200_response() {
        let raw = b"HTTP/1.1 200 Connection Established\r\n\r\n";
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(raw);
        assert_eq!(proc.get_result().unwrap().status_code, 200);
    }

    #[test]
    fn test_parse_407_response() {
        let raw = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\n\r\n";
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(raw);
        let head = proc.get_result().unwrap();
        assert_eq!(head.status_code, 407);
        assert_eq!(head.header("proxy-authenticate"), Some("Basic realm=\"proxy\""));
    }

    #[test]
    fn test_config_default() {
        let config = HttpProxyTunnelConfig::default();
        assert_eq!(config.proxy_port, 8080);
        assert_eq!(config.target_port, 80);
        assert!(config.username.is_none());
    }

    #[test]
    fn test_md5_hex_known_values() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("hello"), "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn test_proxy_type_variants() {
        assert_eq!(HttpProxyType::Tunnel, HttpProxyType::Tunnel);
        assert_ne!(HttpProxyType::Tunnel, HttpProxyType::Forward);
    }

    // =======================================================================
    // Mock-server integration tests
    // =======================================================================

    async fn start_mock_proxy() -> (tokio::net::TcpListener, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn config_for_mock(port: u16) -> HttpProxyTunnelConfig {
        HttpProxyTunnelConfig {
            proxy_host: "127.0.0.1".to_string(),
            proxy_port: port,
            target_host: "target.example.com".to_string(),
            target_port: 443,
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            user_agent: "aria2/rust-test".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_tunnel_success_200() {
        let (listener, port) = start_mock_proxy().await;
        let config = config_for_mock(port);
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = AsyncReadExt::read(&mut sock, &mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("CONNECT target.example.com:443"));
            AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
        });
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
        assert!(result.is_ok(), "Expected tunnel success, got: {:?}", result);
        assert_eq!(result.unwrap().proxy_type, HttpProxyType::Tunnel);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_tunnel_rejected_non_200() {
        let (listener, port) = start_mock_proxy().await;
        let config = config_for_mock(port);
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
            AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 403 Forbidden\r\n\r\n").await.unwrap();
        });
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("403"), "Expected 403, got: {}", msg);
        assert!(msg.contains("rejected CONNECT"), "Expected 'rejected CONNECT', got: {}", msg);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_tunnel_407_basic_auth_success() {
        let (listener, port) = start_mock_proxy().await;
        let config = HttpProxyTunnelConfig {
            username: Some("user".into()), password: Some("pass".into()), ..config_for_mock(port)
        };
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = AsyncReadExt::read(&mut sock, &mut buf).await.unwrap();
            let _req1 = String::from_utf8_lossy(&buf[..n]);
            AsyncWriteExt::write_all(&mut sock,
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n"
            ).await.unwrap();
            let n2 = AsyncReadExt::read(&mut sock, &mut buf).await.unwrap();
            let req2 = String::from_utf8_lossy(&buf[..n2]);
            assert!(req2.contains("Proxy-Authorization: Basic dXNlcjpwYXNz"));
            AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
        });
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
        assert!(result.is_ok(), "Expected auth success, got: {:?}", result);
        assert_eq!(result.unwrap().proxy_type, HttpProxyType::Tunnel);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_tunnel_407_no_credentials_fails() {
        let (listener, port) = start_mock_proxy().await;
        let config = config_for_mock(port);
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
            AsyncWriteExt::write_all(&mut sock,
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n"
            ).await.unwrap();
        });
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("auth"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_tunnel_407_wrong_credentials_fails() {
        let (listener, port) = start_mock_proxy().await;
        let config = HttpProxyTunnelConfig {
            username: Some("wrong".into()), password: Some("creds".into()), ..config_for_mock(port)
        };
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
            AsyncWriteExt::write_all(&mut sock,
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n"
            ).await.unwrap();
            let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
            AsyncWriteExt::write_all(&mut sock,
                b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n"
            ).await.unwrap();
        });
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("auth failed"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_forward_mode_returns_stream() {
        let (listener, port) = start_mock_proxy().await;
        let config = config_for_mock(port);
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
        });
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Forward).await;
        assert!(result.is_ok(), "Expected forward success, got: {:?}", result);
        assert_eq!(result.unwrap().proxy_type, HttpProxyType::Forward);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_tunnel_1xx_then_200() {
        let (listener, port) = start_mock_proxy().await;
        let config = config_for_mock(port);
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
            AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 100 Continue\r\n\r\n").await.unwrap();
            AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
        });
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
        assert!(result.is_ok(), "Expected success after 1xx skip, got: {:?}", result);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_tunnel_connection_refused() {
        let config = HttpProxyTunnelConfig {
            proxy_host: "127.0.0.1".into(), proxy_port: 1,
            target_host: "t.example.com".into(), target_port: 443,
            connect_timeout: Duration::from_millis(500), ..Default::default()
        };
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_tunnel_timeout() {
        let (listener, port) = start_mock_proxy().await;
        let config = HttpProxyTunnelConfig { read_timeout: Duration::from_millis(200), ..config_for_mock(port) };
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
        assert!(result.is_err(), "Expected timeout, got: {:?}", result);
        server.abort();
        let _ = server.await;
    }
}
