//! HTTP request building and method definitions
//!
//! Provides HTTP/1.1 request building with a fluent API, automatic standard
//! headers, and authentication helpers. Mirrors C++ aria2's
//! `HttpRequest::createRequest()` for header generation.

use base64::{Engine, engine::general_purpose};
use std::collections::HashMap;
use url::Url;

use crate::error::Result;

/// HTTP request method enum
#[derive(Debug, Clone, PartialEq)]
pub enum HttpMethod {
    /// GET request method
    Get,
    /// POST request method
    Post,
    /// HEAD request method
    Head,
    /// PUT request method
    Put,
    /// DELETE request method
    Delete,
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpMethod::Get => write!(f, "GET"),
            HttpMethod::Post => write!(f, "POST"),
            HttpMethod::Head => write!(f, "HEAD"),
            HttpMethod::Put => write!(f, "PUT"),
            HttpMethod::Delete => write!(f, "DELETE"),
        }
    }
}

/// HTTP request struct
///
/// Represents a complete HTTP/1.1 request, including method, URL, headers, and optional body.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP request method
    pub method: HttpMethod,
    /// Request URL
    pub url: Url,
    /// Request headers (supports multi-value)
    pub headers: HashMap<String, String>,
    /// Optional request body
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    /// Serialize the HTTP request to raw bytes
    ///
    /// Serializes the request to a byte sequence according to the HTTP/1.1 specification.
    /// Format: `METHOD PATH VERSION\r\nHeaders\r\n\r\nBody`
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut result = String::new();

        // Request line: METHOD /path HTTP/1.1
        let path = self.url.path();
        let query = self.url.query();
        if let Some(q) = query {
            result.push_str(&format!("{} {}?{} HTTP/1.1\r\n", self.method, path, q));
        } else {
            result.push_str(&format!("{} {} HTTP/1.1\r\n", self.method, path));
        }

        // Headers
        for (key, value) in &self.headers {
            result.push_str(&format!("{}: {}\r\n", key, value));
        }

        // Empty line separating header and body
        result.push_str("\r\n");

        let mut bytes = result.into_bytes();

        // Body
        if let Some(ref body) = self.body {
            bytes.extend_from_slice(body);
        }

        bytes
    }
}

/// HTTP request builder (Fluent API)
///
/// Uses a fluent API to build a complete HTTP request, automatically adding
/// standard headers matching C++ aria2's `HttpRequest::createRequest()`.
///
/// # Examples
///
/// ```rust
/// use url::Url;
/// use aria2_core::http::request_response::{HttpRequestBuilder, HttpMethod};
///
/// let url = Url::parse("http://example.com/api").unwrap();
/// let request = HttpRequestBuilder::new(HttpMethod::Get, url)
///     .header("Accept", "application/json")
///     .build()
///     .unwrap();
/// ```
pub struct HttpRequestBuilder {
    /// HTTP method
    method: HttpMethod,
    /// Target URL
    url: Url,
    /// Custom headers
    headers: HashMap<String, String>,
    /// Optional request body
    body: Option<Vec<u8>>,
    /// Enable Accept-Encoding: gzip, deflate (mirrors C++ `acceptGzip_`)
    accept_gzip: bool,
    /// Add Pragma/Cache-Control: no-cache headers (mirrors C++ `noCache_`)
    no_cache: bool,
    /// Add Want-Digest header for integrity checking (mirrors C++ `noWantDigest_` inverted)
    want_digest: bool,
    /// If-Modified-Since header value for conditional GET
    if_modified_since: Option<String>,
    /// If-None-Match header value for ETag-based conditional GET
    if_none_match: Option<String>,
    /// Referer header value
    referer: Option<String>,
    /// Pre-formatted Cookie header string (caller formats from CookieStorage)
    cookie: Option<String>,
    /// Pre-formatted Authorization header value (e.g., from `basic_auth()`)
    authorization: Option<String>,
    /// Pre-formatted Proxy-Authorization header value
    proxy_authorization: Option<String>,
    /// When true, send `Connection: close`; default is keep-alive (HTTP/1.1 default)
    connection_close: bool,
    /// Range start byte offset for resume (mirrors C++ `getStartByte()`)
    range_start: Option<u64>,
    /// Range end byte offset for resume (mirrors C++ `getEndByte()`)
    range_end: Option<u64>,
    /// Whether this is a proxy request that needs `Connection: Keep-Alive`
    /// (mirrors C++ `proxyRequest_` + `isKeepAliveEnabled()`)
    proxy_keep_alive: bool,
}

impl HttpRequestBuilder {
    /// Create a new HTTP request builder
    ///
    /// # Arguments
    ///
    /// * `method` - HTTP request method (GET/POST/HEAD/PUT/DELETE)
    /// * `url` - Target URL
    ///
    /// # Returns
    ///
    /// New HttpRequestBuilder instance with default settings:
    /// - Connection: keep-alive (HTTP/1.1 default, no header emitted)
    /// - All optional headers disabled
    pub fn new(method: HttpMethod, url: Url) -> Self {
        Self {
            method,
            url,
            headers: HashMap::new(),
            body: None,
            accept_gzip: false,
            no_cache: false,
            want_digest: false,
            if_modified_since: None,
            if_none_match: None,
            referer: None,
            cookie: None,
            authorization: None,
            proxy_authorization: None,
            connection_close: false,
            range_start: None,
            range_end: None,
            proxy_keep_alive: false,
        }
    }

    /// Add a single header
    ///
    /// If a header with the same key already exists, it will be overwritten.
    pub fn header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    /// Set headers in batch
    ///
    /// Merges all provided headers into the existing headers.
    /// If duplicate keys exist, new values will overwrite old values.
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers.extend(headers);
        self
    }

    /// Set the request body
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }

    /// Enable Accept-Encoding header for compressed responses
    ///
    /// When enabled, adds `Accept-Encoding: deflate, gzip`.
    /// Mirrors C++ aria2's `contentEncodingEnabled_` + `acceptGzip_` flags.
    pub fn accept_gzip(mut self, enabled: bool) -> Self {
        self.accept_gzip = enabled;
        self
    }

    /// Enable no-cache headers for conditional GET refresh
    ///
    /// When enabled, adds `Pragma: no-cache` and `Cache-Control: no-cache`.
    /// Mirrors C++ aria2's `noCache_` flag.
    pub fn no_cache(mut self, enabled: bool) -> Self {
        self.no_cache = enabled;
        self
    }

    /// Enable Want-Digest header for integrity checking
    ///
    /// When enabled, adds `Want-Digest: SHA-512;q=1, SHA-256;q=1, SHA;q=0.1`.
    /// Mirrors C++ aria2's `!noWantDigest_` flag with the same q-value format.
    pub fn want_digest(mut self, enabled: bool) -> Self {
        self.want_digest = enabled;
        self
    }

    /// Set If-Modified-Since header for conditional GET
    ///
    /// Used for validating cached resources. The server responds with 304
    /// Not Modified if the resource has not changed since the given date.
    /// Mirrors C++ aria2's `ifModSinceHeader_`.
    pub fn if_modified_since(mut self, value: String) -> Self {
        self.if_modified_since = Some(value);
        self
    }

    /// Set If-None-Match header for ETag-based conditional GET
    ///
    /// The server responds with 304 Not Modified if the ETag matches.
    /// In C++ aria2, this is added through the generic header mechanism.
    pub fn if_none_match(mut self, value: String) -> Self {
        self.if_none_match = Some(value);
        self
    }

    /// Set Referer header
    ///
    /// Mirrors C++ aria2's `request_->getReferer()`.
    pub fn referer(mut self, value: String) -> Self {
        self.referer = Some(value);
        self
    }

    /// Auto-derive Referer from the request URL origin
    ///
    /// Sets the Referer header to the origin of the request URL,
    /// matching C++ aria2's default behavior when no explicit referer
    /// is provided by the caller.
    pub fn auto_referer(mut self) -> Self {
        if self.referer.is_none() {
            self.referer = Some(self.url.origin().unicode_serialization());
        }
        self
    }

    /// Set Cookie header from pre-formatted string
    ///
    /// The caller is responsible for formatting cookies from CookieStorage
    /// (matching the URL's domain/path/secure criteria). The string should
    /// be in the format `name1=value1; name2=value2`.
    /// Mirrors C++ aria2's `cookieStorage_->criteriaFind()` flow.
    pub fn cookie(mut self, value: String) -> Self {
        self.cookie = Some(value);
        self
    }

    /// Set Authorization header from pre-formatted value
    ///
    /// Use `basic_auth()` or `bearer_token()` to generate the value.
    /// Mirrors C++ aria2's `authConfig_->getAuthText()` flow.
    ///
    /// TODO: Add support for Digest and NTLM authentication schemes.
    pub fn authorization(mut self, value: String) -> Self {
        self.authorization = Some(value);
        self
    }

    /// Set Proxy-Authorization header from pre-formatted value
    ///
    /// Use `basic_auth()` to generate the value for Basic proxy auth.
    /// Mirrors C++ aria2's `getProxyAuthString()` flow.
    pub fn proxy_authorization(mut self, value: String) -> Self {
        self.proxy_authorization = Some(value);
        self
    }

    /// Set Connection header to close
    ///
    /// When true, emits `Connection: close`. When false (default), no
    /// Connection header is emitted, relying on HTTP/1.1's default keep-alive.
    /// Mirrors C++ aria2's `!request_->isKeepAliveEnabled()` logic.
    pub fn connection_close(mut self, close: bool) -> Self {
        self.connection_close = close;
        self
    }

    /// Set Range header for resume download
    ///
    /// Formats as `Range: bytes=start-end` or `Range: bytes=start-` (open-ended).
    /// Mirrors C++ aria2's segment-based Range header logic in `createRequest()`.
    ///
    /// # Arguments
    ///
    /// * `start` - Start byte offset (inclusive)
    /// * `end` - Optional end byte offset (inclusive). If None, the range is
    ///   open-ended, requesting all bytes from start to end of file.
    pub fn range(mut self, start: u64, end: Option<u64>) -> Self {
        self.range_start = Some(start);
        self.range_end = end;
        self
    }

    /// Enable proxy Connection: Keep-Alive header
    ///
    /// When enabled, emits `Connection: Keep-Alive` for proxy requests.
    /// Mirrors C++ aria2's `proxyRequest_ && isKeepAliveEnabled()` logic.
    pub fn proxy_keep_alive(mut self, enabled: bool) -> Self {
        self.proxy_keep_alive = enabled;
        self
    }

    /// Build the final HTTP request
    ///
    /// Automatically adds standard headers following C++ aria2's
    /// `HttpRequest::createRequest()` logic:
    /// - Host: extracted from URL (omits default ports 80/443)
    /// - User-Agent: aria2-rust/1.0
    /// - Accept: \*/\*
    /// - Accept-Encoding: when `accept_gzip` is enabled
    /// - Pragma/Cache-Control: when `no_cache` is enabled
    /// - Connection: close (when `connection_close`), or Keep-Alive (proxy)
    /// - Range: when `range_start` is set
    /// - Want-Digest: when `want_digest` is enabled (C++ format with q-values)
    /// - If-Modified-Since: when set
    /// - If-None-Match: when set
    /// - Referer: when set
    /// - Cookie: when set
    /// - Authorization: when set
    /// - Proxy-Authorization: when set
    /// - Content-Length: if body is present
    ///
    /// User-set headers always take precedence over auto-generated headers.
    pub fn build(self) -> Result<HttpRequest> {
        let mut final_headers = self.headers;

        // Host header (mirrors C++ getHostText() — omit default ports)
        if !final_headers.contains_key("Host") {
            let host = self.url.host_str().unwrap_or("");
            let port = self.url.port_or_known_default();
            let should_omit_port = match port {
                Some(80) if self.url.scheme() == "http" => true,
                Some(443) if self.url.scheme() == "https" => true,
                _ => false,
            };
            if self.url.port().is_some() && !should_omit_port {
                final_headers.insert(
                    "Host".to_string(),
                    format!("{}:{}", host, self.url.port().unwrap()),
                );
            } else {
                final_headers.insert("Host".to_string(), host.to_string());
            }
        }

        // User-Agent header
        if !final_headers.contains_key("User-Agent") {
            final_headers.insert("User-Agent".to_string(), "aria2-rust/1.0".to_string());
        }

        // Accept header
        if !final_headers.contains_key("Accept") {
            final_headers.insert("Accept".to_string(), "*/*".to_string());
        }

        // Accept-Encoding header (mirrors C++ contentEncodingEnabled_ + acceptGzip_)
        if self.accept_gzip && !final_headers.contains_key("Accept-Encoding") {
            final_headers.insert("Accept-Encoding".to_string(), "deflate, gzip".to_string());
        }

        // Pragma and Cache-Control headers (mirrors C++ noCache_)
        if self.no_cache {
            if !final_headers.contains_key("Pragma") {
                final_headers.insert("Pragma".to_string(), "no-cache".to_string());
            }
            if !final_headers.contains_key("Cache-Control") {
                final_headers.insert("Cache-Control".to_string(), "no-cache".to_string());
            }
        }

        // Connection header (mirrors C++ !isKeepAliveEnabled() && !isPipeliningEnabled())
        // HTTP/1.1 default is keep-alive, so we only emit Connection when explicitly needed
        if !final_headers.contains_key("Connection") {
            if self.connection_close {
                final_headers.insert("Connection".to_string(), "close".to_string());
            } else if self.proxy_keep_alive {
                // Proxy requests with keep-alive: emit Connection: Keep-Alive
                // Mirrors C++: if(proxyRequest_ && isKeepAliveEnabled())
                final_headers.insert("Connection".to_string(), "Keep-Alive".to_string());
            }
            // Otherwise, HTTP/1.1 assumes keep-alive — no header needed.
        }

        // Range header (mirrors C++ segment-based Range logic in createRequest())
        if let Some(start) = self.range_start
            && !final_headers.contains_key("Range")
        {
            let range_value = match self.range_end {
                Some(end) => format!("bytes={}-{}", start, end),
                None => format!("bytes={}-", start),
            };
            final_headers.insert("Range".to_string(), range_value);
        }

        // Want-Digest header (mirrors C++ !noWantDigest_)
        // C++ format: "SHA-512;q=1, SHA-256;q=1, SHA;q=0.1"
        if self.want_digest && !final_headers.contains_key("Want-Digest") {
            final_headers.insert(
                "Want-Digest".to_string(),
                "SHA-512;q=1, SHA-256;q=1, SHA;q=0.1".to_string(),
            );
        }

        // If-Modified-Since header (mirrors C++ ifModSinceHeader_)
        if let Some(ref val) = self.if_modified_since
            && !final_headers.contains_key("If-Modified-Since")
        {
            final_headers.insert("If-Modified-Since".to_string(), val.clone());
        }

        // If-None-Match header (ETag-based conditional GET)
        if let Some(ref val) = self.if_none_match
            && !final_headers.contains_key("If-None-Match")
        {
            final_headers.insert("If-None-Match".to_string(), val.clone());
        }

        // Referer header (mirrors C++ request_->getReferer())
        if let Some(ref val) = self.referer
            && !final_headers.contains_key("Referer")
        {
            final_headers.insert("Referer".to_string(), val.clone());
        }

        // Cookie header (mirrors C++ cookieStorage_->criteriaFind())
        if let Some(ref val) = self.cookie
            && !final_headers.contains_key("Cookie")
        {
            final_headers.insert("Cookie".to_string(), val.clone());
        }

        // Authorization header (mirrors C++ authConfig_->getAuthText())
        if let Some(ref val) = self.authorization
            && !final_headers.contains_key("Authorization")
        {
            final_headers.insert("Authorization".to_string(), val.clone());
        }

        // Proxy-Authorization header (mirrors C++ getProxyAuthString())
        if let Some(ref val) = self.proxy_authorization
            && !final_headers.contains_key("Proxy-Authorization")
        {
            final_headers.insert("Proxy-Authorization".to_string(), val.clone());
        }

        // Content-Length header (if body is present)
        if let Some(body) = &self.body
            && !final_headers.contains_key("Content-Length")
        {
            let len = body.len();
            final_headers.insert("Content-Length".to_string(), len.to_string());
        }

        Ok(HttpRequest {
            method: self.method,
            url: self.url,
            headers: final_headers,
            body: self.body,
        })
    }
}

/// Generate Basic Auth header value
///
/// Encodes username:password as Base64 and returns `Basic <credentials>`.
///
/// # Examples
///
/// ```
/// use aria2_core::http::request_response::basic_auth;
///
/// let auth_header = basic_auth("user", "pass");
/// assert_eq!(auth_header, "Basic dXNlcjpwYXNz");
/// ```
pub fn basic_auth(username: &str, password: &str) -> String {
    let credentials = format!("{}:{}", username, password);
    let encoded = general_purpose::STANDARD.encode(credentials.as_bytes());
    format!("Basic {}", encoded)
}

/// Generate Bearer Token header value
///
/// # Arguments
///
/// * `token` - Bearer token
///
/// # Returns
///
/// Complete Authorization header value (e.g., `Bearer my-token`)
pub fn bearer_token(token: &str) -> String {
    format!("Bearer {}", token)
}
