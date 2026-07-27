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
//! User-Agent: aria2-rust/1.0\r\n
//! Accept: */*\r\n
//! Connection: Keep-Alive\r\n
//! Proxy-Authorization: Basic <credentials>\r\n    (if proxy auth needed)
//! \r\n
//! ```

use std::fmt;
use std::time::Duration;

use tracing::debug;
use url::Url;

use crate::error::Result;

// ---------------------------------------------------------------------------
// ProxyMethod — how to route FTP through an HTTP proxy
// ---------------------------------------------------------------------------

/// How to connect through an HTTP proxy for FTP downloads.
///
/// Mirrors C++ `AbstractCommand::resolveProxyMethod()` which returns
/// either `V_GET` or `V_TUNNEL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
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
            user_agent: "aria2-rust/1.0".to_string(),
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
    /// # Errors
    ///
    /// Returns an error if the FTP URL cannot be serialized.
    pub fn build(self) -> Result<FtpProxyGetRequest> {
        let request_url = self.build_request_url();

        // Build request line with full URL (proxy-style)
        let mut request = format!("GET {} HTTP/1.1\r\n", request_url);

        // Host header: FTP server host (mirrors C++ getHostText(getURIHost(), getPort()))
        let host_header = Self::build_host_header(&self.ftp_url);
        request.push_str(&format!("Host: {}\r\n", host_header));

        // User-Agent header
        request.push_str(&format!("User-Agent: {}\r\n", self.proxy_config.user_agent));

        // Accept header
        request.push_str("Accept: */*\r\n");

        // No-cache headers (mirrors C++ noCache_)
        if self.no_cache {
            request.push_str("Pragma: no-cache\r\n");
            request.push_str("Cache-Control: no-cache\r\n");
        }

        // Connection: Keep-Alive for proxy requests
        // Mirrors C++: if(proxyRequest_ && isKeepAliveEnabled())
        request.push_str("Connection: Keep-Alive\r\n");

        // Range header for resume (mirrors C++ segment-based Range logic)
        if self.range_start > 0 {
            request.push_str(&format!("Range: bytes={}-\r\n", self.range_start));
        }

        // Proxy-Authorization header (mirrors C++ getProxyAuthString())
        if !self.proxy_config.proxy_username.is_empty() {
            let auth_header = build_basic_proxy_auth(
                &self.proxy_config.proxy_username,
                &self.proxy_config.proxy_password,
            );
            request.push_str(&format!("Proxy-Authorization: {}\r\n", auth_header));
        }

        // End of headers
        request.push_str("\r\n");

        debug!(
            "Built FTP proxy GET request ({} bytes) for {}",
            request.len(),
            request_url
        );

        Ok(FtpProxyGetRequest {
            request_bytes: request.into_bytes(),
            request_url,
        })
    }

    /// Build the request URL, possibly embedding FTP username.
    ///
    /// Mirrors C++ `HttpRequest::createRequest()` lines 149-160:
    /// - If proxy is set and protocol is FTP and URL has no username
    ///   but FTP auth credentials exist, insert `USER@` into the URI.
    /// - Otherwise, use the URL as-is.
    fn build_request_url(&self) -> String {
        // Check if the FTP URL already has a username embedded
        let has_embedded_username = !self.ftp_url.username().is_empty();

        if !has_embedded_username && !self.proxy_config.ftp_username.is_empty() {
            // C++: Insert user into URI, like ftp://USER@host/
            // Percent-encode the username per RFC 3986
            let encoded_user = percent_encode_username(&self.proxy_config.ftp_username);
            let url_str = self.ftp_url.to_string();
            // Insert after "ftp://"
            if let Some(pos) = url_str.find("://") {
                let scheme_end = pos + 3;
                let mut result = String::with_capacity(url_str.len() + encoded_user.len() + 1);
                result.push_str(&url_str[..scheme_end]);
                result.push_str(&encoded_user);
                result.push('@');
                result.push_str(&url_str[scheme_end..]);
                return result;
            }
        }

        self.ftp_url.to_string()
    }

    /// Build the Host header value from the FTP URL.
    ///
    /// Mirrors C++ `getHostText(getURIHost(), getPort())`:
    /// - Omits port 21 for FTP (default port)
    /// - Includes port for non-default ports
    fn build_host_header(url: &Url) -> String {
        let host = url.host_str().unwrap_or("");
        let port = url.port();
        match port {
            Some(21) | None => host.to_string(),
            Some(p) => format!("{}:{}", host, p),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Build a Basic Proxy-Authorization header value.
///
/// Mirrors C++ `HttpRequest::getProxyAuthString()`.
fn build_basic_proxy_auth(username: &str, password: &str) -> String {
    use base64::Engine;
    let credentials = format!("{}:{}", username, password);
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
    format!("Basic {}", encoded)
}

/// Percent-encode a username for embedding in a URL.
///
/// Mirrors C++ `util::percentEncode()` for the FTP username.
/// Encodes all characters except unreserved (RFC 3986 section 2.3).
fn percent_encode_username(username: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                                 abcdefghijklmnopqrstuvwxyz\
                                 0123456789\
                                 -._~";
    let mut result = String::with_capacity(username.len());
    for byte in username.bytes() {
        if UNRESERVED.contains(&byte) {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{:02X}", byte));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build request and return as string for assertions.
    fn build_request_string(ftp_url: Url, proxy_config: FtpProxyConfig) -> String {
        let result = FtpProxyGetRequestBuilder::new(ftp_url, proxy_config)
            .build()
            .unwrap();
        String::from_utf8(result.request_bytes).unwrap()
    }

    #[test]
    fn test_proxy_method_default() {
        assert_eq!(ProxyMethod::default(), ProxyMethod::Get);
    }

    #[test]
    fn test_proxy_method_display() {
        assert_eq!(format!("{}", ProxyMethod::Get), "GET");
        assert_eq!(format!("{}", ProxyMethod::Tunnel), "TUNNEL");
    }

    #[test]
    fn test_resolve_proxy_method_default_ftp() {
        let config = FtpProxyConfig::default();
        assert_eq!(resolve_proxy_method(&config, "ftp"), ProxyMethod::Get);
    }

    #[test]
    fn test_resolve_proxy_method_explicit_tunnel() {
        let config = FtpProxyConfig {
            explicit_proxy_method: Some(ProxyMethod::Tunnel),
            ..Default::default()
        };
        assert_eq!(resolve_proxy_method(&config, "ftp"), ProxyMethod::Tunnel);
    }

    #[test]
    fn test_resolve_proxy_method_https_always_tunnel() {
        let config = FtpProxyConfig::default();
        assert_eq!(resolve_proxy_method(&config, "https"), ProxyMethod::Tunnel);
    }

    #[test]
    fn test_resolve_proxy_method_sftp_always_tunnel() {
        let config = FtpProxyConfig::default();
        assert_eq!(resolve_proxy_method(&config, "sftp"), ProxyMethod::Tunnel);
    }

    #[test]
    fn test_resolve_proxy_method_https_ignores_explicit_get() {
        // Even if someone explicitly sets GET, HTTPS always uses Tunnel
        let config = FtpProxyConfig {
            explicit_proxy_method: Some(ProxyMethod::Get),
            ..Default::default()
        };
        assert_eq!(resolve_proxy_method(&config, "https"), ProxyMethod::Tunnel);
    }

    #[test]
    fn test_ftp_proxy_config_default() {
        let config = FtpProxyConfig::default();
        assert_eq!(config.proxy_port, 8080);
        assert!(config.proxy_username.is_empty());
        assert!(config.proxy_password.is_empty());
        assert!(config.ftp_username.is_empty());
        assert!(config.explicit_proxy_method.is_none());
    }

    #[test]
    fn test_build_basic_proxy_auth() {
        let header = build_basic_proxy_auth("proxyuser", "proxypass");
        assert!(header.starts_with("Basic "));
        // Base64 of "proxyuser:proxypass"
        assert_eq!(header, "Basic cHJveHl1c2VyOnByb3h5cGFzcw==");
    }

    #[test]
    fn test_percent_encode_username_simple() {
        assert_eq!(percent_encode_username("user"), "user");
    }

    #[test]
    fn test_percent_encode_username_special_chars() {
        assert_eq!(percent_encode_username("user@domain"), "user%40domain");
        assert_eq!(percent_encode_username("user name"), "user%20name");
        assert_eq!(percent_encode_username("user/pass"), "user%2Fpass");
    }

    #[test]
    fn test_build_request_simple() {
        let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
        let proxy_config = FtpProxyConfig {
            proxy_host: "proxy.example.com".to_string(),
            proxy_port: 8080,
            user_agent: "aria2-rust/1.0".to_string(),
            ..Default::default()
        };
        let s = build_request_string(ftp_url, proxy_config);
        assert!(s.starts_with("GET ftp://ftp.example.com/pub/file.tar.gz HTTP/1.1\r\n"));
        assert!(s.contains("Host: ftp.example.com\r\n"));
        assert!(s.contains("User-Agent: aria2-rust/1.0\r\n"));
        assert!(s.contains("Connection: Keep-Alive\r\n"));
        assert!(!s.contains("Proxy-Authorization"));
        assert!(!s.contains("Range:"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_build_request_with_proxy_auth() {
        let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
        let proxy_config = FtpProxyConfig {
            proxy_username: "proxyuser".to_string(),
            proxy_password: "proxypass".to_string(),
            ..Default::default()
        };
        let s = build_request_string(ftp_url, proxy_config);
        assert!(s.contains("Proxy-Authorization: Basic cHJveHl1c2VyOnByb3h5cGFzcw==\r\n"));
    }

    #[test]
    fn test_build_request_with_ftp_username_embedded() {
        let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
        let proxy_config = FtpProxyConfig {
            ftp_username: "ftpuser".to_string(),
            ..Default::default()
        };
        let s = build_request_string(ftp_url, proxy_config);
        assert!(s.contains("GET ftp://ftpuser@ftp.example.com/pub/file.tar.gz HTTP/1.1\r\n"));
    }

    #[test]
    fn test_build_request_no_double_username() {
        let ftp_url = Url::parse("ftp://existing@ftp.example.com/pub/file.tar.gz").unwrap();
        let proxy_config = FtpProxyConfig {
            ftp_username: "ftpuser".to_string(),
            ..Default::default()
        };
        let s = build_request_string(ftp_url, proxy_config);
        assert!(s.contains("ftp://existing@ftp.example.com"));
        assert!(!s.contains("ftp://ftpuser@"));
    }

    #[test]
    fn test_build_request_with_range() {
        let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
        let result = FtpProxyGetRequestBuilder::new(ftp_url, FtpProxyConfig::default())
            .range_start(1024)
            .build()
            .unwrap();
        let s = String::from_utf8(result.request_bytes).unwrap();
        assert!(s.contains("Range: bytes=1024-\r\n"));
    }

    #[test]
    fn test_build_request_with_no_cache() {
        let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
        let result = FtpProxyGetRequestBuilder::new(ftp_url, FtpProxyConfig::default())
            .no_cache(true)
            .build()
            .unwrap();
        let s = String::from_utf8(result.request_bytes).unwrap();
        assert!(s.contains("Pragma: no-cache\r\n"));
        assert!(s.contains("Cache-Control: no-cache\r\n"));
    }

    #[test]
    fn test_build_host_header_default_port() {
        let url = Url::parse("ftp://ftp.example.com/pub/file").unwrap();
        assert_eq!(
            FtpProxyGetRequestBuilder::build_host_header(&url),
            "ftp.example.com"
        );
    }

    #[test]
    fn test_build_host_header_non_default_port() {
        let url = Url::parse("ftp://ftp.example.com:2121/pub/file").unwrap();
        assert_eq!(
            FtpProxyGetRequestBuilder::build_host_header(&url),
            "ftp.example.com:2121"
        );
    }

    #[test]
    fn test_build_request_full_round_trip() {
        let ftp_url = Url::parse("ftp://ftp.example.com/pub/linux/file.tar.gz").unwrap();
        let proxy_config = FtpProxyConfig {
            proxy_host: "proxy.local".to_string(),
            proxy_port: 3128,
            proxy_username: "puser".to_string(),
            proxy_password: "ppass".to_string(),
            ftp_username: "anonymous".to_string(),
            user_agent: "aria2-rust/2.0".to_string(),
            ..Default::default()
        };

        let result = FtpProxyGetRequestBuilder::new(ftp_url, proxy_config)
            .range_start(4096)
            .no_cache(true)
            .build()
            .unwrap();

        let s = String::from_utf8(result.request_bytes).unwrap();
        assert!(
            s.contains("GET ftp://anonymous@ftp.example.com/pub/linux/file.tar.gz HTTP/1.1\r\n")
        );
        assert!(s.contains("Host: ftp.example.com\r\n"));
        assert!(s.contains("User-Agent: aria2-rust/2.0\r\n"));
        assert!(s.contains("Pragma: no-cache\r\n"));
        assert!(s.contains("Connection: Keep-Alive\r\n"));
        assert!(s.contains("Range: bytes=4096-\r\n"));
        assert!(s.contains("Proxy-Authorization: Basic"));
        assert!(s.ends_with("\r\n\r\n"));
        assert_eq!(
            result.request_url,
            "ftp://anonymous@ftp.example.com/pub/linux/file.tar.gz"
        );
    }
}
