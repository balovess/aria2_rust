//! FTP-over-HTTP-proxy GET method
//!
//! When an HTTP proxy is configured for FTP downloads, the default behavior
//! (V_GET in C++ aria2) is to send the FTP URL as a plain HTTP GET request
//! through the proxy. The proxy itself speaks FTP to the target server and
//! returns the data over HTTP.
//!
//! This is distinct from the CONNECT tunnel mode (`proxy_tunnel.rs`), where
//! a tunnel is established and the client speaks FTP directly.
//!
//! # C++ Equivalent
//!
//! Mirrors the V_GET path in `FtpInitiateConnectionCommand::createNextCommandProxied()`:
//! - `getRequest()->setMethod(Request::METHOD_GET)`
//! - Uses `HttpRequestConnectChain` (normal HTTP download pipeline)
//! - `HttpRequest::createRequest()` emits `GET ftp://host/path HTTP/1.1`
//!
//! # Request Format
//!
//! ```text
//! GET ftp://ftp.example.com/pub/file.tar.gz HTTP/1.1\r\n
//! Host: ftp.example.com\r\n
//! User-Agent: aria2-rust/<release-version>\r\n
//! Accept: */*\r\n
//! Connection: Keep-Alive\r\n
//! Proxy-Authorization: Basic <credentials>\r\n    (if proxy auth needed)
//! \r\n
//! ```

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::network::OutboundNetworkPolicy;
use tracing::debug;
use url::Url;

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::{HttpHeaderProcessor, HttpResponseHead};

// ---------------------------------------------------------------------------
// ProxyMethod — how to route FTP through an HTTP proxy
// ---------------------------------------------------------------------------

/// How to connect through an HTTP proxy for FTP downloads.
///
/// Mirrors C++ `AbstractCommand::resolveProxyMethod()` which returns
/// either `V_GET` or `V_TUNNEL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyMethod {
    /// Send FTP URL as HTTP GET through proxy (default for FTP).
    ///
    /// The proxy translates `ftp://` to FTP and returns data over HTTP.
    /// Equivalent to C++ `V_GET`.
    #[default]
    Get,
    /// Establish CONNECT tunnel, then speak FTP directly.
    /// Already implemented in `proxy_tunnel.rs`.
    /// Equivalent to C++ `V_TUNNEL`.
    Tunnel,
}

impl fmt::Display for ProxyMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProxyMethod::Get => write!(f, "GET"),
            ProxyMethod::Tunnel => write!(f, "TUNNEL"),
        }
    }
}

impl ProxyMethod {
    /// Parse the wire spelling accepted by aria2's `proxy-method` option.
    ///
    /// Keeping this conversion next to the enum prevents each protocol
    /// adapter from inventing a slightly different string policy.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "get" => Some(Self::Get),
            "tunnel" => Some(Self::Tunnel),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// FtpProxyConfig — shared proxy configuration
// ---------------------------------------------------------------------------

/// Configuration for FTP proxy routing (shared by both GET and Tunnel modes).
///
/// This struct contains all the information needed to decide which proxy
/// method to use and to construct the appropriate request.
#[derive(Debug, Clone)]
pub struct FtpProxyConfig {
    /// Proxy server hostname
    pub proxy_host: String,
    /// Proxy server port
    pub proxy_port: u16,
    /// Proxy authentication username (empty if no auth)
    pub proxy_username: String,
    /// Proxy authentication password (empty if no auth)
    pub proxy_password: String,
    /// FTP server authentication username (for embedding in URI)
    pub ftp_username: String,
    /// Connection timeout for connecting to the proxy
    pub connect_timeout: Duration,
    /// User-Agent header to send
    pub user_agent: String,
    /// Explicit proxy method override (from --proxy-method option).
    /// When set to `Some(Tunnel)`, forces CONNECT tunnel instead of GET.
    pub explicit_proxy_method: Option<ProxyMethod>,
}

impl Default for FtpProxyConfig {
    fn default() -> Self {
        Self {
            proxy_host: String::new(),
            proxy_port: 8080,
            proxy_username: String::new(),
            proxy_password: String::new(),
            ftp_username: String::new(),
            connect_timeout: Duration::from_secs(30),
            user_agent: crate::constants::USER_AGENT.to_string(),
            explicit_proxy_method: None,
        }
    }
}

// ---------------------------------------------------------------------------
// resolve_proxy_method — mirrors C++ AbstractCommand::resolveProxyMethod()
// ---------------------------------------------------------------------------

/// Resolve which proxy method to use for FTP.
///
/// Mirrors C++ `AbstractCommand::resolveProxyMethod()`:
/// - If `--proxy-method=tunnel` is explicitly set, use Tunnel
/// - For `https` or `sftp` protocols, always use Tunnel
/// - Otherwise, use Get (the default for FTP)
///
/// # Arguments
///
/// * `config` - Proxy configuration with optional explicit method override
/// * `protocol` - The protocol being proxied ("ftp", "https", "sftp")
///
/// # Returns
///
/// The resolved `ProxyMethod`.
pub fn resolve_proxy_method(config: &FtpProxyConfig, protocol: &str) -> ProxyMethod {
    // C++: if protocol == "https" || protocol == "sftp" -> V_TUNNEL
    if protocol.eq_ignore_ascii_case("https") || protocol.eq_ignore_ascii_case("sftp") {
        debug!("Protocol {} requires Tunnel proxy method", protocol);
        return ProxyMethod::Tunnel;
    }

    // C++: if PREF_PROXY_METHOD == V_TUNNEL -> V_TUNNEL
    match config.explicit_proxy_method {
        Some(ProxyMethod::Tunnel) => {
            debug!("Explicit proxy method override: Tunnel");
            ProxyMethod::Tunnel
        }
        _ => {
            // Default for FTP: V_GET
            debug!("Default proxy method for FTP: Get");
            ProxyMethod::Get
        }
    }
}

// ---------------------------------------------------------------------------
// FtpProxyGetRequest — constructs the HTTP GET request for proxy
// ---------------------------------------------------------------------------

/// Result of building an FTP-over-proxy GET request.
///
/// Contains the raw HTTP/1.1 request bytes ready to write to the proxy socket.
#[derive(Debug, Clone)]
pub struct FtpProxyGetRequest {
    /// The raw HTTP/1.1 request bytes (including final \r\n\r\n)
    pub request_bytes: Vec<u8>,
    /// The full FTP URL used in the request line
    pub request_url: String,
}

/// HTTP response returned by an FTP-over-HTTP forward proxy.
///
/// The response head is parsed with the shared Rust HTTP parser. Bytes read
/// together with the head are retained so the payload cannot be lost between
/// header parsing and the FTP command's disk writer.
pub struct FtpProxyGetResponse {
    pub head: HttpResponseHead,
    stream: TcpStream,
    buffered_body: Vec<u8>,
}

impl FtpProxyGetResponse {
    /// Read payload bytes after the HTTP response head.
    pub async fn read_body(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.buffered_body.is_empty() {
            let count = buffer.len().min(self.buffered_body.len());
            buffer[..count].copy_from_slice(&self.buffered_body[..count]);
            self.buffered_body.drain(..count);
            return Ok(count);
        }
        self.stream.read(buffer).await
    }
}

/// Execute one HTTP forward-proxy GET request for an FTP URL.
///
/// This is the Rust equivalent of aria2's `V_GET` path. The proxy receives
/// the absolute `ftp://` request target and returns the payload as an HTTP
/// response; the FTP control channel is intentionally not opened in this
/// mode.
pub async fn execute_proxy_get(
    ftp_url: Url,
    proxy_config: &FtpProxyConfig,
    range_start: u64,
    no_cache: bool,
) -> Result<FtpProxyGetResponse> {
    execute_proxy_get_with_policy(
        ftp_url,
        proxy_config,
        range_start,
        no_cache,
        &OutboundNetworkPolicy::direct(),
    )
    .await
}

/// Execute a forward-proxy FTP GET through the shared outbound policy.
pub async fn execute_proxy_get_with_policy(
    ftp_url: Url,
    proxy_config: &FtpProxyConfig,
    range_start: u64,
    no_cache: bool,
    policy: &OutboundNetworkPolicy,
) -> Result<FtpProxyGetResponse> {
    let request = FtpProxyGetRequestBuilder::new(ftp_url, proxy_config.clone())
        .range_start(range_start)
        .no_cache(no_cache)
        .build()?;

    let connect = async {
        if policy.is_direct() {
            TcpStream::connect((proxy_config.proxy_host.as_str(), proxy_config.proxy_port))
                .await
                .map_err(|error| Aria2Error::Network(error.to_string()))
        } else {
            policy
                .connect_host(&proxy_config.proxy_host, proxy_config.proxy_port)
                .await
                .map_err(|error| Aria2Error::Network(error.to_string()))
        }
    };
    let mut stream = tokio::time::timeout(proxy_config.connect_timeout, connect)
        .await
        .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
        .map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!(
                    "FTP proxy connection failed to {}:{}: {}",
                    proxy_config.proxy_host, proxy_config.proxy_port, error
                ),
            })
        })?;
    stream.set_nodelay(true).map_err(|error| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("FTP proxy set_nodelay failed: {}", error),
        })
    })?;

    tokio::time::timeout(
        proxy_config.connect_timeout,
        stream.write_all(&request.request_bytes),
    )
    .await
    .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
    .map_err(|error| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("FTP proxy request failed: {}", error),
        })
    })?;
    stream.flush().await.map_err(|error| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("FTP proxy request flush failed: {}", error),
        })
    })?;

    let mut processor = HttpHeaderProcessor::new();
    let mut read_buffer = [0u8; 4096];
    let mut body_prefix = Vec::new();
    loop {
        let bytes_read =
            tokio::time::timeout(proxy_config.connect_timeout, stream.read(&mut read_buffer))
                .await
                .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
                .map_err(|error| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP proxy response failed: {}", error),
                    })
                })?;
        if bytes_read == 0 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "FTP proxy closed before response headers".into(),
                },
            ));
        }

        let state = processor.feed(&read_buffer[..bytes_read]).clone();
        let header_bytes = processor.last_bytes_processed();
        if state.is_complete() {
            if header_bytes < bytes_read {
                body_prefix.extend_from_slice(&read_buffer[header_bytes..bytes_read]);
            }
            let head = processor.get_result()?;
            debug!(status = head.status_code, "Received FTP proxy GET response");
            return Ok(FtpProxyGetResponse {
                head,
                stream,
                buffered_body: body_prefix,
            });
        }
        if state.is_error() {
            return Err(Aria2Error::HttpProtocol(
                "Invalid FTP proxy response headers".into(),
            ));
        }
    }
}

/// Builder for FTP-over-proxy GET requests.
///
/// Constructs the raw HTTP/1.1 request sent to the proxy in V_GET mode.
/// The request line contains the full FTP URL (not just the path),
/// telling the proxy which FTP resource to fetch.
///
/// Key differences from normal HTTP GET:
/// 1. Request line uses the full FTP URL: `GET ftp://host/path HTTP/1.1`
/// 2. `Host` header contains the FTP server host, not the proxy host
/// 3. `Connection: Keep-Alive` is added for proxy keep-alive
/// 4. `Proxy-Authorization` header is added if proxy credentials exist
/// 5. FTP username is inserted into the URI: `ftp://USER@host/path`
///    when the URL has no embedded username but auth credentials exist
pub struct FtpProxyGetRequestBuilder {
    /// The original FTP URL (e.g., ftp://ftp.example.com/pub/file.tar.gz)
    ftp_url: Url,
    /// Proxy configuration
    proxy_config: FtpProxyConfig,
    /// Resume offset (Range header), 0 means no Range header
    range_start: u64,
    /// Whether to add Pragma/Cache-Control: no-cache headers
    no_cache: bool,
}

impl FtpProxyGetRequestBuilder {
    /// Create a new builder for the given FTP URL and proxy config.
    ///
    /// # Arguments
    ///
    /// * `ftp_url` - The original FTP URL to download
    /// * `proxy_config` - Proxy server configuration
    pub fn new(ftp_url: Url, proxy_config: FtpProxyConfig) -> Self {
        Self {
            ftp_url,
            proxy_config,
            range_start: 0,
            no_cache: false,
        }
    }

    /// Set the resume offset for Range header.
    ///
    /// When `start > 0`, adds `Range: bytes=start-` header.
    /// Mirrors C++ `HttpRequest::createRequest()` segment-based Range logic.
    pub fn range_start(mut self, start: u64) -> Self {
        self.range_start = start;
        self
    }

    /// Enable no-cache headers for conditional GET refresh.
    ///
    /// When enabled, adds `Pragma: no-cache` and `Cache-Control: no-cache`.
    pub fn no_cache(mut self, enabled: bool) -> Self {
        self.no_cache = enabled;
        self
    }

    /// Build the raw HTTP/1.1 GET request for the proxy.
    ///
    /// Constructs the complete request including:
    /// - Request line with full FTP URL
    /// - Standard headers (Host, User-Agent, Accept)
    /// - Proxy-specific headers (Connection, Proxy-Authorization)
    /// - Conditional headers (Range, Cache-Control)
    /// - FTP auth embedded in URI if needed
    ///
    /// The `Result` return type is retained for compatibility with the public
    /// core API; it also reports invalid configured HTTP field values.
    pub fn build(self) -> Result<FtpProxyGetRequest> {
        let request = aria2_protocol::ftp::proxy::try_build_get_request(
            &self.ftp_url,
            &aria2_protocol::ftp::proxy::ProxyGetRequestOptions {
                ftp_username: &self.proxy_config.ftp_username,
                proxy_username: &self.proxy_config.proxy_username,
                proxy_password: &self.proxy_config.proxy_password,
                user_agent: &self.proxy_config.user_agent,
                range_start: self.range_start,
                no_cache: self.no_cache,
            },
        )
        .map_err(|error| match error {
            aria2_protocol::ftp::proxy::ProxyGetRequestError::InvalidUserAgent => {
                Aria2Error::InvalidArgument(
                    "user-agent contains invalid HTTP field characters".to_string(),
                )
            }
            aria2_protocol::ftp::proxy::ProxyGetRequestError::InvalidRequest(_) => {
                Aria2Error::InvalidArgument("invalid FTP proxy HTTP request fields".to_string())
            }
        })?;
        Ok(FtpProxyGetRequest {
            request_bytes: request.request_bytes,
            request_url: request.request_url,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
