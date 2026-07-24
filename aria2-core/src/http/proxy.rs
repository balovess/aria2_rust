//! HTTP proxy support (CONNECT tunnel and forward proxy)
//!
//! Implements HTTP proxy functionality matching the C++ aria2
//! `AbstractProxyRequestCommand`, `AbstractProxyResponseCommand`,
//! `HttpProxyRequestCommand`, and `HttpProxyResponseCommand`.
//!
//! Two proxy modes are supported:
//!
//! - **CONNECT tunnel** ([`HttpProxyTunnel`]): For HTTPS downloads, sends
//!   `CONNECT host:port HTTP/1.1` to the proxy, which establishes a blind
//!   TCP tunnel. The returned `TcpStream` can then be used for TLS.
//!
//! - **Forward proxy** ([`HttpProxyForward`]): For HTTP downloads, sends the
//!   request with the full URL (e.g., `GET http://host:port/path HTTP/1.1`).
//!   The proxy relays the request and response.
//!
//! Both modes support proxy authentication (407 Proxy Authentication Required)
//! via Basic and Digest schemes, reusing the existing [`DigestAuthChallenge`]
//! and [`basic_auth`] infrastructure.

use std::time::Duration;

use base64::{Engine, engine::general_purpose};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, Result};
use crate::http::digest_auth::{DigestAuthChallenge, DigestAuthResponse};
use crate::http::header_processor::{HttpHeaderProcessor, HttpResponseHead};
use crate::http::socks_connector::ProxyUrl;

// ---------------------------------------------------------------------------
// HttpProxyConfig
// ---------------------------------------------------------------------------

/// Configuration for an HTTP proxy connection.
///
/// Can be constructed directly or parsed from a proxy URL string via
/// [`HttpProxyConfig::from_proxy_url`].
#[derive(Debug, Clone)]
pub struct HttpProxyConfig {
    /// Proxy server hostname or IP address
    pub proxy_host: String,
    /// Proxy server port
    pub proxy_port: u16,
    /// Optional proxy username for authentication
    pub proxy_username: Option<String>,
    /// Optional proxy password for authentication
    pub proxy_password: Option<String>,
    /// Target server hostname we want to reach through the proxy
    pub target_host: String,
    /// Target server port we want to reach through the proxy
    pub target_port: u16,
    /// Timeout for establishing the TCP connection to the proxy
    pub connect_timeout: Duration,
    /// Timeout for reading data from the proxy
    pub read_timeout: Duration,
    /// Timeout for writing data to the proxy
    pub write_timeout: Duration,
}

impl HttpProxyConfig {
    /// Create a new proxy config with default timeouts (30s connect, 60s read/write).
    pub fn new(proxy_host: String, proxy_port: u16, target_host: String, target_port: u16) -> Self {
        Self {
            proxy_host,
            proxy_port,
            proxy_username: None,
            proxy_password: None,
            target_host,
            target_port,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(60),
        }
    }

    /// Set proxy authentication credentials.
    pub fn with_credentials(mut self, username: String, password: String) -> Self {
        self.proxy_username = Some(username);
        self.proxy_password = Some(password);
        self
    }

    /// Parse a proxy URL string (e.g., `http://user:pass@host:port`) into a config.
    ///
    /// Uses the existing [`ProxyUrl`] parser from the `socks_connector` module.
    /// Only HTTP and HTTPS proxy protocols are supported; SOCKS URLs return an error.
    pub fn from_proxy_url(
        proxy_url: &str,
        target_host: String,
        target_port: u16,
    ) -> Result<Self> {
        let parsed = ProxyUrl::parse(proxy_url).map_err(|e| {
            Aria2Error::Parse(format!("Invalid proxy URL '{}': {}", proxy_url, e))
        })?;

        match parsed.protocol {
            crate::http::socks_connector::ProxyProtocol::Http
            | crate::http::socks_connector::ProxyProtocol::Https => {}
            _ => {
                return Err(Aria2Error::Parse(format!(
                    "Expected http/https proxy URL, got: {:?}",
                    parsed.protocol
                )));
            }
        }

        let mut config = Self::new(parsed.host, parsed.port, target_host, target_port);

        if let Some(user) = parsed.username {
            config.proxy_username = Some(user);
        }
        if let Some(pass) = parsed.password {
            config.proxy_password = Some(pass);
        }

        Ok(config)
    }

    /// The `host:port` string for the target (used in CONNECT and Host headers).
    fn target_host_port(&self) -> String {
        format!("{}:{}", self.target_host, self.target_port)
    }
}

// ---------------------------------------------------------------------------
// ProxyResponse
// ---------------------------------------------------------------------------

/// Result of parsing a proxy's HTTP response during CONNECT/forward handshake.
#[derive(Debug, Clone)]
pub enum ProxyResponse {
    /// Tunnel/forward connection established successfully (HTTP 200).
    /// The proxy response headers are included for inspection.
    Connected(HttpResponseHead),
    /// Proxy requires authentication (HTTP 407).
    /// Contains the `Proxy-Authenticate` challenge header value(s).
    AuthRequired {
        /// The parsed response head (contains Proxy-Authenticate headers)
        response: HttpResponseHead,
    },
    /// Proxy returned an error status code.
    Error {
        /// HTTP status code
        status_code: u16,
        /// Reason phrase
        reason: String,
    },
}

impl ProxyResponse {
    /// Classify an [`HttpResponseHead`] from a proxy into a [`ProxyResponse`].
    fn from_head(head: HttpResponseHead) -> Self {
        match head.status_code {
            200 => ProxyResponse::Connected(head),
            407 => ProxyResponse::AuthRequired { response: head },
            code => ProxyResponse::Error {
                status_code: code,
                reason: head.reason_phrase.clone(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Auth header builders
// ---------------------------------------------------------------------------

/// Build a `Proxy-Authorization: Basic ...` header value.
fn proxy_basic_auth(username: &str, password: &str) -> String {
    let credentials = format!("{}:{}", username, password);
    let encoded = general_purpose::STANDARD.encode(credentials.as_bytes());
    format!("Basic {}", encoded)
}

/// Build a `Proxy-Authorization: Digest ...` header value using the existing
/// [`DigestAuthResponse`] infrastructure.
fn proxy_digest_auth(
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    challenge: &DigestAuthChallenge,
    nc: u32,
) -> String {
    let response = DigestAuthResponse::compute(username, password, method, uri, challenge, nc);
    response.to_header_value()
}

/// Parse `Proxy-Authenticate` headers from a response and build the
/// appropriate `Proxy-Authorization` header value for retry.
///
/// Returns `None` if no supported scheme is found or if credentials are missing.
fn build_proxy_auth_header(
    head: &HttpResponseHead,
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    nc: u32,
) -> Option<String> {
    // Check for Digest first (more secure), then Basic
    for (_, value) in head.iter_headers() {
        if value.starts_with("Digest ") || value.starts_with("digest ") {
            if let Ok(challenge) = DigestAuthChallenge::parse(value) {
                let auth = proxy_digest_auth(username, password, method, uri, &challenge, nc);
                debug!("Using Digest proxy authentication for realm='{}'", challenge.realm);
                return Some(auth);
            }
        }
    }

    // Fall back to Basic
    for (_, value) in head.iter_headers() {
        if value.starts_with("Basic ") || value.starts_with("basic ") {
            let auth = proxy_basic_auth(username, password);
            debug!("Using Basic proxy authentication");
            return Some(auth);
        }
    }

    // If Proxy-Authenticate exists but scheme is unknown, try Basic as last resort
    if head.header("proxy-authenticate").is_some() {
        warn!("Unknown Proxy-Authenticate scheme, falling back to Basic");
        return Some(proxy_basic_auth(username, password));
    }

    None
}

// ---------------------------------------------------------------------------
// I/O helpers
// ---------------------------------------------------------------------------

/// Buffer size for reading proxy response headers.
const READ_BUF_SIZE: usize = 4096;

/// Maximum number of auth retry attempts (prevent infinite loops on bad creds).
const MAX_AUTH_RETRIES: u32 = 2;

/// Read the proxy's HTTP response head using the streaming [`HttpHeaderProcessor`].
///
/// Applies the config's `read_timeout` to each read operation.
async fn read_proxy_response(
    stream: &mut TcpStream,
    read_timeout: Duration,
) -> Result<HttpResponseHead> {
    let mut processor = HttpHeaderProcessor::new();
    let mut buf = [0u8; READ_BUF_SIZE];

    loop {
        let n = tokio::time::timeout(read_timeout, stream.read(&mut buf))
            .await
            .map_err(|_| Aria2Error::Network("Timeout reading proxy response".to_string()))?
            .map_err(|e| Aria2Error::Network(format!("Error reading proxy response: {}", e)))?;

        if n == 0 {
            return Err(Aria2Error::Network(
                "Connection closed by proxy before response complete".to_string(),
            ));
        }

        let state = processor.feed(&buf[..n]);
        if state.is_complete() {
            return processor.get_result();
        }
        if state.is_error() {
            return Err(Aria2Error::Parse(format!(
                "Error parsing proxy response: {}",
                state.is_error()
            )));
        }
    }
}

/// Write all bytes to the stream with a timeout.
async fn write_all_timeout(
    stream: &mut TcpStream,
    data: &[u8],
    write_timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(write_timeout, stream.write_all(data))
        .await
        .map_err(|_| Aria2Error::Network("Timeout writing to proxy".to_string()))?
        .map_err(|e| Aria2Error::Network(format!("Error writing to proxy: {}", e)))?;
    Ok(())
}

/// Connect to the proxy TCP endpoint with a timeout.
async fn connect_to_proxy(config: &HttpProxyConfig) -> Result<TcpStream> {
    let addr = format!("{}:{}", config.proxy_host, config.proxy_port);
    debug!("Connecting to proxy at {}", addr);

    let stream = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| {
            Aria2Error::Network(format!(
                "Timeout connecting to proxy {} ({}s)",
                addr,
                config.connect_timeout.as_secs()
            ))
        })?
        .map_err(|e| {
            Aria2Error::Network(format!(
                "Failed to connect to proxy '{}': {}",
                addr, e
            ))
        })?;

    info!("Connected to proxy at {}", addr);
    Ok(stream)
}

// ---------------------------------------------------------------------------
// HttpProxyTunnel
// ---------------------------------------------------------------------------

/// HTTP CONNECT tunnel through a proxy for HTTPS downloads.
///
/// The flow is:
/// 1. Connect to the proxy server
/// 2. Send `CONNECT target_host:target_port HTTP/1.1\r\nHost: ...\r\n\r\n`
/// 3. If 407 received, retry with `Proxy-Authorization` header
/// 4. If 200 received, the tunnel is established and the `TcpStream` is returned
///    for the caller to perform TLS handshake on
///
/// # Example
///
/// ```rust,ignore
/// use aria2_core::http::proxy::{HttpProxyConfig, HttpProxyTunnel};
///
/// let config = HttpProxyConfig::new(
///     "proxy.example.com".into(), 3128,
///     "target.example.com".into(), 443,
/// );
/// let tunnel = HttpProxyTunnel::new(config);
/// let stream = tunnel.connect().await?;
/// // Now perform TLS handshake on `stream`
/// ```
pub struct HttpProxyTunnel {
    config: HttpProxyConfig,
}

impl HttpProxyTunnel {
    /// Create a new CONNECT tunnel handler with the given configuration.
    pub fn new(config: HttpProxyConfig) -> Self {
        Self { config }
    }

    /// Establish an HTTP CONNECT tunnel through the proxy.
    ///
    /// On success, returns the `TcpStream` which is now tunneled — bytes
    /// written to / read from it go directly to/from the target server.
    /// The caller should perform TLS handshake on this stream for HTTPS.
    pub async fn connect(&self) -> Result<TcpStream> {
        let mut stream = connect_to_proxy(&self.config).await?;

        let target = self.config.target_host_port();
        let mut auth_nc = 1u32;

        // Initial CONNECT request (no auth)
        let request = self.build_connect_request(None);
        debug!("Sending CONNECT request to proxy for {}", target);
        write_all_timeout(&mut stream, request.as_bytes(), self.config.write_timeout).await?;

        loop {
            let head = read_proxy_response(&mut stream, self.config.read_timeout).await?;
            let proxy_resp = ProxyResponse::from_head(head);

            match proxy_resp {
                ProxyResponse::Connected(head) => {
                    info!(
                        "CONNECT tunnel established to {} via proxy",
                        target
                    );
                    debug!("Proxy response: {:?}", head);
                    return Ok(stream);
                }
                ProxyResponse::AuthRequired { response } => {
                    let (username, password) = self.get_credentials()?;

                    if auth_nc > MAX_AUTH_RETRIES {
                        return Err(Aria2Error::Network(
                            "Proxy authentication failed after max retries".to_string(),
                        ));
                    }

                    // Build Proxy-Authorization header for the CONNECT method
                    let auth_value = build_proxy_auth_header(
                        &response,
                        &username,
                        &password,
                        "CONNECT",
                        &target,
                        auth_nc,
                    ).ok_or_else(|| {
                        Aria2Error::Network(
                            "Proxy requires auth but no supported scheme found".to_string(),
                        )
                    })?;

                    auth_nc += 1;
                    warn!("Proxy returned 407, retrying CONNECT with authentication (attempt {})", auth_nc);

                    // Re-send CONNECT with Proxy-Authorization
                    let request = self.build_connect_request(Some(&auth_value));
                    write_all_timeout(&mut stream, request.as_bytes(), self.config.write_timeout).await?;
                }
                ProxyResponse::Error { status_code, reason } => {
                    return Err(Aria2Error::Network(format!(
                        "Proxy returned error {} {} for CONNECT to {}",
                        status_code, reason, target
                    )));
                }
            }
        }
    }

    /// Build the CONNECT request string.
    fn build_connect_request(&self, proxy_auth: Option<&str>) -> String {
        let target = self.config.target_host_port();
        let mut req = format!(
            "CONNECT {} HTTP/1.1\r\nHost: {}\r\n",
            target, target
        );

        if let Some(auth) = proxy_auth {
            req.push_str(&format!("Proxy-Authorization: {}\r\n", auth));
        }

        // Proxy-Connection: keep-alive is standard for CONNECT tunnels
        req.push_str("Proxy-Connection: keep-alive\r\n");
        req.push_str("\r\n");
        req
    }

    /// Extract credentials or return an error.
    fn get_credentials(&self) -> Result<(String, String)> {
        match (&self.config.proxy_username, &self.config.proxy_password) {
            (Some(u), Some(p)) => Ok((u.clone(), p.clone())),
            (Some(u), None) => Ok((u.clone(), String::new())),
            _ => Err(Aria2Error::Network(
                "Proxy requires authentication but no credentials provided".to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// HttpProxyForward
// ---------------------------------------------------------------------------

/// HTTP forward proxy for non-HTTPS downloads.
///
/// In forward mode, the proxy acts as a relay. The client sends requests with
/// the full URL (e.g., `GET http://target:port/path HTTP/1.1`) instead of just
/// the path. The proxy forwards the request to the target and relays the response.
///
/// For proxy authentication (407), the `Proxy-Authorization` header is added
/// on retry.
///
/// # Example
///
/// ```rust,ignore
/// use aria2_core::http::proxy::{HttpProxyConfig, HttpProxyForward};
///
/// let config = HttpProxyConfig::new(
///     "proxy.example.com".into(), 3128,
///     "target.example.com".into(), 80,
/// );
/// let forward = HttpProxyForward::new(config);
/// // Send the initial request and handle 407 retry
/// let stream = forward.connect().await?;
/// // Now send the actual HTTP request with full URL through `stream`
/// ```
pub struct HttpProxyForward {
    config: HttpProxyConfig,
}

impl HttpProxyForward {
    /// Create a new forward proxy handler with the given configuration.
    pub fn new(config: HttpProxyConfig) -> Self {
        Self { config }
    }

    /// Connect to the proxy and verify it is reachable.
    ///
    /// For forward proxy, we simply establish the TCP connection to the proxy.
    /// The actual HTTP request with the full URL is sent by the caller on the
    /// returned stream. This method also performs a lightweight handshake check:
    /// it sends an HTTP HEAD request and handles 407 if needed, then returns
    /// the stream ready for the actual request.
    ///
    /// If `skip_handshake` is true, only the TCP connection is established
    /// without any probe request. This is useful when the caller will
    /// immediately send their own request.
    pub async fn connect(&self, skip_handshake: bool) -> Result<TcpStream> {
        let mut stream = connect_to_proxy(&self.config).await?;

        if skip_handshake {
            return Ok(stream);
        }

        // Send a probe HEAD request to check if proxy auth is needed
        let target_url = format!(
            "http://{}:{}",
            self.config.target_host, self.config.target_port
        );
        let probe_request = self.build_forward_request("HEAD", &target_url, "/", None);
        debug!("Sending probe HEAD request to proxy for {}", target_url);
        write_all_timeout(&mut stream, probe_request.as_bytes(), self.config.write_timeout).await?;

        let mut auth_nc = 1u32;

        loop {
            let head = read_proxy_response(&mut stream, self.config.read_timeout).await?;
            let proxy_resp = ProxyResponse::from_head(head);

            match proxy_resp {
                ProxyResponse::Connected(_) => {
                    info!("Forward proxy connection ready for {}", target_url);
                    // For a HEAD probe, a 200 means we're good.
                    // But the stream has consumed the response; we need a fresh
                    // connection for the actual data request.
                    drop(stream);
                    return connect_to_proxy(&self.config).await;
                }
                ProxyResponse::AuthRequired { response } => {
                    let (username, password) = self.get_credentials()?;

                    if auth_nc > MAX_AUTH_RETRIES {
                        return Err(Aria2Error::Network(
                            "Proxy authentication failed after max retries".to_string(),
                        ));
                    }

                    let auth_value = build_proxy_auth_header(
                        &response,
                        &username,
                        &password,
                        "HEAD",
                        &target_url,
                        auth_nc,
                    ).ok_or_else(|| {
                        Aria2Error::Network(
                            "Proxy requires auth but no supported scheme found".to_string(),
                        )
                    })?;

                    auth_nc += 1;
                    warn!("Proxy returned 407, retrying with authentication (attempt {})", auth_nc);

                    // Close this connection and open a new one for the retry
                    drop(stream);
                    stream = connect_to_proxy(&self.config).await?;

                    let retry_request = self.build_forward_request("HEAD", &target_url, "/", Some(&auth_value));
                    write_all_timeout(&mut stream, retry_request.as_bytes(), self.config.write_timeout).await?;
                }
                ProxyResponse::Error { status_code, reason } => {
                    // Some proxies return 403 or other errors for HEAD probes
                    // but work fine for actual GET requests. Log a warning
                    // and return the stream anyway so the caller can try.
                    warn!(
                        "Proxy returned {} {} on probe, returning stream for caller to retry",
                        status_code, reason
                    );
                    drop(stream);
                    return connect_to_proxy(&self.config).await;
                }
            }
        }
    }

    /// Build a forward proxy request with the full URL.
    ///
    /// In forward proxy mode, the request line uses the absolute URL:
    /// `METHOD http://host:port/path HTTP/1.1`
    ///
    /// # Arguments
    /// * `method` - HTTP method (GET, HEAD, etc.)
    /// * `full_url` - The full URL including scheme and host (e.g., `http://target:80/path`)
    /// * `path` - The path component (used for Digest auth URI)
    /// * `proxy_auth` - Optional `Proxy-Authorization` header value
    pub fn build_forward_request(
        &self,
        method: &str,
        full_url: &str,
        path: &str,
        proxy_auth: Option<&str>,
    ) -> String {
        let mut req = format!("{} {} HTTP/1.1\r\n", method, full_url);
        req.push_str(&format!("Host: {}\r\n", self.config.target_host_port()));

        if self.config.target_port != 80 {
            // Already included in Host above if non-standard
        }

        if let Some(auth) = proxy_auth {
            req.push_str(&format!("Proxy-Authorization: {}\r\n", auth));
        }

        req.push_str(&format!(
            "User-Agent: aria2-rust/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        ));

        let _ = path; // Path is used by the caller for Digest auth URI computation
        req
    }

    /// Extract credentials or return an error.
    fn get_credentials(&self) -> Result<(String, String)> {
        match (&self.config.proxy_username, &self.config.proxy_password) {
            (Some(u), Some(p)) => Ok((u.clone(), p.clone())),
            (Some(u), None) => Ok((u.clone(), String::new())),
            _ => Err(Aria2Error::Network(
                "Proxy requires authentication but no credentials provided".to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Free helper: send a request through a forward proxy and handle 407
// ---------------------------------------------------------------------------

/// Send an HTTP GET request through a forward proxy, handling 407 auth retry.
///
/// This is a convenience function that combines proxy connection, request
/// sending, and auth handling into a single call. On success, returns the
/// `TcpStream` positioned after the response headers (ready to read body),
/// along with the parsed [`HttpResponseHead`].
///
/// # Arguments
/// * `config` - Proxy configuration
/// * `path` - The request path on the target (e.g., `/download/file.zip`)
///
/// # Returns
/// A tuple of `(TcpStream, HttpResponseHead)` where the stream is ready
/// for reading the response body.
pub async fn forward_get_with_auth(
    config: &HttpProxyConfig,
    path: &str,
) -> Result<(TcpStream, HttpResponseHead)> {
    let full_url = format!(
        "http://{}:{}{}",
        config.target_host, config.target_port, path
    );

    let mut stream = connect_to_proxy(config).await?;
    let mut auth_nc = 1u32;
    let mut current_auth: Option<String> = None;

    loop {
        let request = if let Some(ref auth) = current_auth {
            format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: {}\r\nUser-Agent: aria2-rust/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n",
                full_url,
                config.target_host_port(),
                auth
            )
        } else {
            format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: aria2-rust/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n",
                full_url,
                config.target_host_port()
            )
        };

        write_all_timeout(&mut stream, request.as_bytes(), config.write_timeout).await?;
        let head = read_proxy_response(&mut stream, config.read_timeout).await?;
        let proxy_resp = ProxyResponse::from_head(head);

        match proxy_resp {
            ProxyResponse::Connected(head) => {
                return Ok((stream, head));
            }
            ProxyResponse::AuthRequired { response } => {
                let username = config.proxy_username.as_deref().unwrap_or("");
                let password = config.proxy_password.as_deref().unwrap_or("");

                if username.is_empty() && config.proxy_username.is_none() {
                    return Err(Aria2Error::Network(
                        "Proxy requires authentication but no credentials provided".to_string(),
                    ));
                }

                if auth_nc > MAX_AUTH_RETRIES {
                    return Err(Aria2Error::Network(
                        "Proxy authentication failed after max retries".to_string(),
                    ));
                }

                let auth_value = build_proxy_auth_header(
                    &response,
                    username,
                    password,
                    "GET",
                    &full_url,
                    auth_nc,
                ).ok_or_else(|| {
                    Aria2Error::Network(
                        "Proxy requires auth but no supported scheme found".to_string(),
                    )
                })?;

                auth_nc += 1;
                current_auth = Some(auth_value);

                warn!("Proxy returned 407, retrying GET with auth (attempt {})", auth_nc);

                // Open a new connection for the retry
                drop(stream);
                stream = connect_to_proxy(config).await?;
            }
            ProxyResponse::Error { status_code, reason } => {
                return Err(Aria2Error::Network(format!(
                    "Proxy returned error {} {} for GET {}",
                    status_code, reason, full_url
                )));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== HttpProxyConfig tests ====================

    #[test]
    fn test_proxy_config_new() {
        let config = HttpProxyConfig::new(
            "proxy.example.com".into(),
            3128,
            "target.example.com".into(),
            443,
        );
        assert_eq!(config.proxy_host, "proxy.example.com");
        assert_eq!(config.proxy_port, 3128);
        assert_eq!(config.target_host, "target.example.com");
        assert_eq!(config.target_port, 443);
        assert!(config.proxy_username.is_none());
        assert!(config.proxy_password.is_none());
        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(config.read_timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_proxy_config_with_credentials() {
        let config = HttpProxyConfig::new("p".into(), 8080, "t".into(), 80)
            .with_credentials("user".into(), "pass".into());
        assert_eq!(config.proxy_username.as_deref(), Some("user"));
        assert_eq!(config.proxy_password.as_deref(), Some("pass"));
    }

    #[test]
    fn test_proxy_config_from_proxy_url_http() {
        let config = HttpProxyConfig::from_proxy_url(
            "http://admin:secret@proxy.corp.com:3128",
            "target.com".into(),
            443,
        )
        .unwrap();
        assert_eq!(config.proxy_host, "proxy.corp.com");
        assert_eq!(config.proxy_port, 3128);
        assert_eq!(config.proxy_username.as_deref(), Some("admin"));
        assert_eq!(config.proxy_password.as_deref(), Some("secret"));
        assert_eq!(config.target_host, "target.com");
        assert_eq!(config.target_port, 443);
    }

    #[test]
    fn test_proxy_config_from_proxy_url_https() {
        let config = HttpProxyConfig::from_proxy_url(
            "https://secure.proxy.com",
            "target.com".into(),
            443,
        )
        .unwrap();
        assert_eq!(config.proxy_host, "secure.proxy.com");
        assert_eq!(config.proxy_port, 443); // default HTTPS port
    }

    #[test]
    fn test_proxy_config_from_proxy_url_no_credentials() {
        let config = HttpProxyConfig::from_proxy_url(
            "http://proxy.local:8080",
            "t".into(),
            80,
        )
        .unwrap();
        assert!(config.proxy_username.is_none());
        assert!(config.proxy_password.is_none());
    }

    #[test]
    fn test_proxy_config_from_proxy_url_socks_rejected() {
        let result = HttpProxyConfig::from_proxy_url(
            "socks5://proxy.local:1080",
            "t".into(),
            80,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("http/https"));
    }

    #[test]
    fn test_proxy_config_target_host_port() {
        let config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443);
        assert_eq!(config.target_host_port(), "t:443");
    }

    // ==================== ProxyResponse tests ====================

    #[test]
    fn test_proxy_response_200_connected() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 Connection established\r\n\r\n");
        let head = proc.get_result().unwrap();

        let resp = ProxyResponse::from_head(head);
        match resp {
            ProxyResponse::Connected(h) => {
                assert_eq!(h.status_code, 200);
            }
            _ => panic!("Expected Connected, got {:?}", resp),
        }
    }

    #[test]
    fn test_proxy_response_407_auth_required() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"Proxy\"\r\n\r\n");
        let head = proc.get_result().unwrap();

        let resp = ProxyResponse::from_head(head);
        match resp {
            ProxyResponse::AuthRequired { response } => {
                assert_eq!(response.status_code, 407);
                assert_eq!(
                    response.header("proxy-authenticate"),
                    Some("Basic realm=\"Proxy\"")
                );
            }
            _ => panic!("Expected AuthRequired, got {:?}", resp),
        }
    }

    #[test]
    fn test_proxy_response_error_status() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 403 Forbidden\r\n\r\n");
        let head = proc.get_result().unwrap();

        let resp = ProxyResponse::from_head(head);
        match resp {
            ProxyResponse::Error { status_code, reason } => {
                assert_eq!(status_code, 403);
                assert_eq!(reason, "Forbidden");
            }
            _ => panic!("Expected Error, got {:?}", resp),
        }
    }

    #[test]
    fn test_proxy_response_500_error() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
        let head = proc.get_result().unwrap();

        let resp = ProxyResponse::from_head(head);
        match resp {
            ProxyResponse::Error { status_code, reason } => {
                assert_eq!(status_code, 500);
                assert_eq!(reason, "Internal Server Error");
            }
            _ => panic!("Expected Error, got {:?}", resp),
        }
    }

    // ==================== Auth header builder tests ====================

    #[test]
    fn test_proxy_basic_auth_encoding() {
        let auth = proxy_basic_auth("user", "pass");
        assert!(auth.starts_with("Basic "));
        // Verify Base64: "user:pass" -> "dXNlcjpwYXNz"
        assert_eq!(auth, "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn test_proxy_basic_auth_special_chars() {
        let auth = proxy_basic_auth("admin@corp", "p@ss:w0rd");
        assert!(auth.starts_with("Basic "));
        // Decode to verify
        let encoded = &auth["Basic ".len()..];
        let decoded = String::from_utf8(
            general_purpose::STANDARD
                .decode(encoded)
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert_eq!(decoded, "admin@corp:p@ss:w0rd");
    }

    #[test]
    fn test_build_proxy_auth_header_basic() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: Basic realm=\"Proxy\"\r\n\r\n");
        let head = proc.get_result().unwrap();

        let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
        assert!(auth.is_some());
        let auth = auth.unwrap();
        assert!(auth.starts_with("Basic "));
    }

    #[test]
    fn test_build_proxy_auth_header_digest() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: Digest realm=\"Proxy\", nonce=\"abc123\", qop=\"auth\"\r\n\r\n");
        let head = proc.get_result().unwrap();

        let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
        assert!(auth.is_some());
        let auth = auth.unwrap();
        assert!(auth.starts_with("Digest "));
        assert!(auth.contains(r#"username="user""#));
    }

    #[test]
    fn test_build_proxy_auth_header_digest_preferred_over_basic() {
        // When both Digest and Basic are offered, Digest should be preferred
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: Basic realm=\"Proxy\"\r\nProxy-Authenticate: Digest realm=\"Proxy\", nonce=\"abc123\", qop=\"auth\"\r\n\r\n");
        let head = proc.get_result().unwrap();

        let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
        assert!(auth.is_some());
        let auth = auth.unwrap();
        // Digest should be preferred over Basic
        assert!(auth.starts_with("Digest "));
    }

    #[test]
    fn test_build_proxy_auth_header_no_auth_header() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 407 Auth\r\n\r\n");
        let head = proc.get_result().unwrap();

        let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
        // No Proxy-Authenticate header -> None
        assert!(auth.is_none());
    }

    // ==================== HttpProxyTunnel request building tests ====================

    #[test]
    fn test_tunnel_build_connect_request_no_auth() {
        let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 443);
        let tunnel = HttpProxyTunnel::new(config);
        let req = tunnel.build_connect_request(None);

        assert!(req.starts_with("CONNECT target.com:443 HTTP/1.1\r\n"));
        assert!(req.contains("Host: target.com:443\r\n"));
        assert!(req.contains("Proxy-Connection: keep-alive\r\n"));
        assert!(!req.contains("Proxy-Authorization"));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_tunnel_build_connect_request_with_basic_auth() {
        let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 443);
        let tunnel = HttpProxyTunnel::new(config);
        let req = tunnel.build_connect_request(Some("Basic dXNlcjpwYXNz"));

        assert!(req.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
    }

    #[test]
    fn test_tunnel_build_connect_request_with_digest_auth() {
        let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 443);
        let tunnel = HttpProxyTunnel::new(config);
        let digest_value = r#"Digest username="admin", realm="Proxy", nonce="abc", uri="target.com:443", nc=00000001, cnonce="x", qop="auth", response="h", algorithm="MD5", opaque="o""#;
        let req = tunnel.build_connect_request(Some(digest_value));

        assert!(req.contains("Proxy-Authorization: Digest "));
        assert!(req.contains(r#"username="admin""#));
    }

    #[test]
    fn test_tunnel_get_credentials() {
        let config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443)
            .with_credentials("user".into(), "pass".into());
        let tunnel = HttpProxyTunnel::new(config);
        let (u, p) = tunnel.get_credentials().unwrap();
        assert_eq!(u, "user");
        assert_eq!(p, "pass");
    }

    #[test]
    fn test_tunnel_get_credentials_missing() {
        let config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443);
        let tunnel = HttpProxyTunnel::new(config);
        assert!(tunnel.get_credentials().is_err());
    }

    #[test]
    fn test_tunnel_get_credentials_username_only() {
        let mut config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443);
        config.proxy_username = Some("user".into());
        let tunnel = HttpProxyTunnel::new(config);
        let (u, p) = tunnel.get_credentials().unwrap();
        assert_eq!(u, "user");
        assert_eq!(p, "");
    }

    // ==================== HttpProxyForward request building tests ====================

    #[test]
    fn test_forward_build_request_no_auth() {
        let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 80);
        let forward = HttpProxyForward::new(config);
        let req = forward.build_forward_request(
            "GET",
            "http://target.com:80/path",
            "/path",
            None,
        );

        assert!(req.starts_with("GET http://target.com:80/path HTTP/1.1\r\n"));
        assert!(req.contains("Host: target.com:80\r\n"));
        assert!(!req.contains("Proxy-Authorization"));
        assert!(req.contains("User-Agent: aria2-rust/1.0\r\n"));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_forward_build_request_with_auth() {
        let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 80);
        let forward = HttpProxyForward::new(config);
        let req = forward.build_forward_request(
            "GET",
            "http://target.com:80/file.zip",
            "/file.zip",
            Some("Basic dXNlcjpwYXNz"),
        );

        assert!(req.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
    }

    #[test]
    fn test_forward_build_head_request() {
        let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 80);
        let forward = HttpProxyForward::new(config);
        let req = forward.build_forward_request(
            "HEAD",
            "http://target.com:80/",
            "/",
            None,
        );

        assert!(req.starts_with("HEAD http://target.com:80/ HTTP/1.1\r\n"));
    }

    #[test]
    fn test_forward_get_credentials() {
        let config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 80)
            .with_credentials("admin".into(), "s3cret".into());
        let forward = HttpProxyForward::new(config);
        let (u, p) = forward.get_credentials().unwrap();
        assert_eq!(u, "admin");
        assert_eq!(p, "s3cret");
    }

    // ==================== proxy_digest_auth tests ====================

    #[test]
    fn test_proxy_digest_auth_produces_header() {
        let challenge = DigestAuthChallenge::parse(
            r#"Digest realm="Proxy", nonce="abc123", qop="auth""#,
        )
        .unwrap();

        let auth = proxy_digest_auth("user", "pass", "CONNECT", "target.com:443", &challenge, 1);
        assert!(auth.starts_with("Digest "));
        assert!(auth.contains(r#"username="user""#));
        assert!(auth.contains(r#"realm="Proxy""#));
        assert!(auth.contains("nc=00000001"));
    }

    // ==================== ProxyUrl integration tests ====================

    #[test]
    fn test_proxy_url_http_default_port() {
        let parsed = ProxyUrl::parse("http://proxy.local").unwrap();
        assert_eq!(parsed.port, 8080);
    }

    #[test]
    fn test_proxy_url_https_default_port() {
        let parsed = ProxyUrl::parse("https://proxy.local").unwrap();
        assert_eq!(parsed.port, 443);
    }

    #[test]
    fn test_proxy_url_with_auth() {
        let parsed = ProxyUrl::parse("http://u:p@proxy.local:3128").unwrap();
        assert_eq!(parsed.username, Some("u".to_string()));
        assert_eq!(parsed.password, Some("p".to_string()));
    }

    // ==================== Timeout configuration tests ====================

    #[test]
    fn test_custom_timeouts() {
        let mut config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443);
        config.connect_timeout = Duration::from_secs(10);
        config.read_timeout = Duration::from_secs(30);
        config.write_timeout = Duration::from_secs(30);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.read_timeout, Duration::from_secs(30));
        assert_eq!(config.write_timeout, Duration::from_secs(30));
    }

    // ==================== Edge case tests ====================

    #[test]
    fn test_proxy_response_non_standard_2xx() {
        // Some proxies return 200 with different reason phrases
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 Tunnel Connection Established\r\n\r\n");
        let head = proc.get_result().unwrap();

        let resp = ProxyResponse::from_head(head);
        assert!(matches!(resp, ProxyResponse::Connected(_)));
    }

    #[test]
    fn test_build_proxy_auth_header_fallback_to_basic() {
        // Unknown scheme should fall back to Basic
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: NTLM\r\n\r\n");
        let head = proc.get_result().unwrap();

        let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
        assert!(auth.is_some());
        let auth = auth.unwrap();
        // Should fall back to Basic
        assert!(auth.starts_with("Basic "));
    }

    #[test]
    fn test_tunnel_connect_request_format_complete() {
        let config = HttpProxyConfig::new("proxy.local".into(), 8080, "github.com".into(), 443);
        let tunnel = HttpProxyTunnel::new(config);
        let req = tunnel.build_connect_request(None);

        // Verify exact format
        let expected = "CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\nProxy-Connection: keep-alive\r\n\r\n";
        assert_eq!(req, expected);
    }

    #[test]
    fn test_forward_request_target_port_80() {
        let config = HttpProxyConfig::new("proxy.local".into(), 8080, "example.com".into(), 80);
        let forward = HttpProxyForward::new(config);
        let req = forward.build_forward_request(
            "GET",
            "http://example.com:80/index.html",
            "/index.html",
            None,
        );

        assert!(req.starts_with("GET http://example.com:80/index.html HTTP/1.1\r\n"));
        assert!(req.contains("Host: example.com:80\r\n"));
    }
}
