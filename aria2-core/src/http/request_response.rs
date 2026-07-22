//! HTTP request building and response parsing module
//!
//! Provides HTTP/1.1 request building, response parsing, and authentication.
//! Supports fluent API for building HTTP requests with automatic standard headers.

use base64::{Engine, engine::general_purpose};
use std::collections::HashMap;
use url::Url;

use crate::error::{Aria2Error, Result};

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
    ///
    /// # Returns
    ///
    /// Serialized byte array
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
/// Uses a fluent API to build a complete HTTP request, automatically adding standard headers.
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
    /// New HttpRequestBuilder instance
    pub fn new(method: HttpMethod, url: Url) -> Self {
        Self {
            method,
            url,
            headers: HashMap::new(),
            body: None,
        }
    }

    /// Add a single header
    ///
    /// If a header with the same key already exists, it will be overwritten.
    ///
    /// # Arguments
    ///
    /// * `key` - Header name
    /// * `value` - Header value
    ///
    /// # Returns
    ///
    /// Self, supporting chained calls
    pub fn header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    /// Set headers in batch
    ///
    /// Merges all provided headers into the existing headers.
    /// If duplicate keys exist, new values will overwrite old values.
    ///
    /// # Arguments
    ///
    /// * `headers` - Headers collection to add
    ///
    /// # Returns
    ///
    /// Self, supporting chained calls
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers.extend(headers);
        self
    }

    /// Set the request body
    ///
    /// # Arguments
    ///
    /// * `body` - Byte data of the request body
    ///
    /// # Returns
    ///
    /// Self, supporting chained calls
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }

    /// Build the final HTTP request
    ///
    /// Automatically adds the following standard headers:
    /// - Host: extracted from URL
    /// - User-Agent: aria2-rust/1.0
    /// - Accept: */*
    /// - Connection: close
    /// - Content-Length: if body is present
    ///
    /// # Returns
    ///
    /// The built HttpRequest, or an error message
    pub fn build(self) -> Result<HttpRequest> {
        let mut final_headers = self.headers;

        // Automatically add standard headers (if not manually set by user)
        // Host header
        if !final_headers.contains_key("Host") {
            let host = self.url.host_str().unwrap_or("");
            if let Some(port) = self.url.port() {
                final_headers.insert("Host".to_string(), format!("{}:{}", host, port));
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

        // Connection header
        if !final_headers.contains_key("Connection") {
            final_headers.insert("Connection".to_string(), "close".to_string());
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

/// HTTP response struct
///
/// Represents a complete HTTP response, including status code, reason phrase, version, headers, and optional body.
/// Supports multi-value headers (e.g., Set-Cookie).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code (e.g., 200, 404, 301)
    pub status_code: u16,
    /// Reason phrase (e.g., OK, Not Found, Moved Permanently)
    pub reason_phrase: String,
    /// HTTP version (e.g., "HTTP/1.1")
    pub version: String,
    /// Response headers (supports multi-value)
    pub headers: HashMap<String, Vec<String>>,
    /// Optional response body
    pub body: Option<Vec<u8>>,
}

impl HttpResponse {
    /// Parse HTTP response from raw bytes
    ///
    /// Parses response data conforming to the HTTP/1.1 specification, including status line, headers, and body.
    /// Supports multi-value headers (via comma separation or multiple headers with the same name).
    ///
    /// # Arguments
    ///
    /// * `data` - Raw HTTP response bytes
    ///
    /// # Returns
    ///
    /// Parsed HttpResponse, or an error message
    ///
    /// # Errors
    ///
    /// Returns an error if the response format is invalid or cannot be parsed
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let response_str = String::from_utf8(data.to_vec())
            .map_err(|e| Aria2Error::Parse(format!("Invalid UTF-8 in HTTP response: {}", e)))?;

        // Separate headers and body
        let (header_part, body_part) = match response_str.find("\r\n\r\n") {
            Some(pos) => (&response_str[..pos], &response_str[pos + 4..]),
            None => (response_str.as_str(), ""),
        };

        // Parse status line
        let mut lines = header_part.split("\r\n");
        let status_line = lines
            .next()
            .ok_or_else(|| Aria2Error::Parse("Empty HTTP response".to_string()))?;

        // Parse version, status_code, reason_phrase
        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(Aria2Error::Parse(
                "Invalid HTTP status line format".to_string(),
            ));
        }

        let version = parts[0].to_string();
        let status_code: u16 = parts[1]
            .parse()
            .map_err(|e| Aria2Error::Parse(format!("Invalid status code: {}", e)))?;
        let reason_phrase = if parts.len() > 2 {
            parts[2..].join(" ")
        } else {
            String::new()
        };

        // Parse headers (supports multi-value)
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim().to_string();
                let value = value.trim().to_string();
                headers.entry(key).or_default().push(value);
            }
        }

        // Process body
        let body = if body_part.is_empty() {
            None
        } else {
            Some(body_part.as_bytes().to_vec())
        };

        Ok(HttpResponse {
            status_code,
            reason_phrase,
            version,
            headers,
            body,
        })
    }

    /// Get the first value of a specified header
    ///
    /// # Arguments
    ///
    /// * `name` - Header name (case-insensitive)
    ///
    /// # Returns
    ///
    /// Reference to the first header value, or None if not found
    pub fn header(&self, name: &str) -> Option<&String> {
        let name_lower = name.to_lowercase();
        for (key, values) in &self.headers {
            if key.to_lowercase() == name_lower {
                return values.first();
            }
        }
        None
    }

    /// Get all values of a specified header
    ///
    /// Particularly useful for headers like Set-Cookie that may appear multiple times.
    ///
    /// # Arguments
    ///
    /// * `name` - Header name (case-insensitive)
    ///
    /// # Returns
    ///
    /// Vector containing all matching values
    pub fn header_all(&self, name: &str) -> Vec<String> {
        let name_lower = name.to_lowercase();
        for (key, values) in &self.headers {
            if key.to_lowercase() == name_lower {
                return values.clone();
            }
        }
        Vec::new()
    }

    /// Get the value of the Content-Length header
    ///
    /// # Returns
    ///
    /// Content length (u64), or None if not present or parsing fails
    pub fn content_length(&self) -> Option<u64> {
        self.header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
    }

    /// Check if this is a redirect response (3xx)
    ///
    /// # Returns
    ///
    /// true if the status code is in the 300-399 range
    pub fn is_redirect(&self) -> bool {
        (300..400).contains(&self.status_code)
    }

    /// Get the Location header and parse it as a URL
    ///
    /// Particularly useful for redirect responses. If it is a relative URL, it will be resolved based on the current request URL.
    ///
    /// # Returns
    ///
    /// Parsed absolute URL, or None if not present or parsing fails
    pub fn location(&self) -> Option<Url> {
        self.header("Location").and_then(|loc| Url::parse(loc).ok())
    }

    /// Get the decoded body using streaming decoders
    ///
    /// Automatically selects appropriate decoders based on the HTTP response's Content-Encoding and Transfer-Encoding headers
    /// to decode the response body. Supports GZip, Chunked, BZip2, and other encoding formats.
    ///
    /// Follows RFC 7230 Section 3.3.1: Transfer-Encoding takes precedence over Content-Encoding.
    ///
    /// # Returns
    ///
    /// Decoded raw data, or an error message. Returns an empty vector if no body is present.
    ///
    /// # Errors
    ///
    /// - If the encoding format is invalid or the data is corrupted
    /// - If an I/O error occurs during decoding
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let response = /* HTTP response with Content-Encoding: gzip */;
    /// let decoded = response.decoded_body()?;
    /// // decoded contains the decompressed raw data
    /// ```
    pub fn decoded_body(&self) -> Result<Vec<u8>> {
        use crate::http::stream_filter::{AutoFilterSelector, process_filters};

        let encoding = self.header("Content-Encoding").map(|s| s.as_str());
        let transfer_enc = self.header("Transfer-Encoding").map(|s| s.as_str());

        let mut filters = AutoFilterSelector::select_filters(encoding, transfer_enc);

        match &self.body {
            Some(raw_data) => process_filters(&mut filters, raw_data),
            None => Ok(Vec::new()),
        }
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

// Base64 encoding is imported via use base64::{engine::general_purpose, Engine}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_builder_fluent_api() {
        let url = Url::parse("http://example.com/api/test").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Post, url.clone())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(b"{\"key\":\"value\"}".to_vec())
            .build()
            .unwrap();

        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.url, url);
        assert_eq!(
            request.headers.get("Content-Type").unwrap(),
            "application/json"
        );
        assert_eq!(request.headers.get("Accept").unwrap(), "application/json");
        assert!(request.body.is_some());
        assert_eq!(request.body.unwrap(), b"{\"key\":\"value\"}");
    }

    #[test]
    fn test_request_auto_headers_generation() {
        let url = Url::parse("http://example.com:8080/path").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Get, url)
            .build()
            .unwrap();

        // Verify auto-generated Host header (with port)
        assert_eq!(request.headers.get("Host").unwrap(), "example.com:8080");

        // Verify auto-generated User-Agent
        assert_eq!(request.headers.get("User-Agent").unwrap(), "aria2-rust/1.0");

        // Verify auto-generated Accept
        assert_eq!(request.headers.get("Accept").unwrap(), "*/*");

        // Verify auto-generated Connection
        assert_eq!(request.headers.get("Connection").unwrap(), "close");
    }

    #[test]
    fn test_request_auto_content_length() {
        let url = Url::parse("http://example.com/api").unwrap();
        let body = b"test body data";
        let request = HttpRequestBuilder::new(HttpMethod::Post, url)
            .body(body.to_vec())
            .build()
            .unwrap();

        assert_eq!(
            request.headers.get("Content-Length").unwrap(),
            &body.len().to_string()
        );
    }

    #[test]
    fn test_request_custom_host_not_overridden() {
        let url = Url::parse("http://example.com/api").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Get, url)
            .header("Host", "custom-host.com")
            .build()
            .unwrap();

        // User-defined Host should be preserved
        assert_eq!(request.headers.get("Host").unwrap(), "custom-host.com");
    }

    #[test]
    fn test_request_to_bytes() {
        let url = Url::parse("http://example.com/path?q=1").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Get, url)
            .header("Custom-Header", "test-value")
            .build()
            .unwrap();

        let bytes = request.to_bytes();
        let request_str = String::from_utf8(bytes).unwrap();

        // Verify request line
        assert!(request_str.starts_with("GET /path?q=1 HTTP/1.1\r\n"));

        // Verify custom header
        assert!(request_str.contains("Custom-Header: test-value"));

        // Verify standard headers
        assert!(request_str.contains("Host: example.com"));
        assert!(request_str.contains("User-Agent: aria2-rust/1.0"));
    }

    #[test]
    fn test_request_to_bytes_with_body() {
        let url = Url::parse("http://example.com/api").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Post, url)
            .header("Content-Type", "text/plain")
            .body(b"Hello, World!".to_vec())
            .build()
            .unwrap();

        let bytes = request.to_bytes();
        let request_str = String::from_utf8_lossy(&bytes);

        assert!(request_str.contains("POST /api HTTP/1.1"));
        assert!(request_str.contains("Content-Length: 13"));
        assert!(request_str.ends_with("Hello, World!"));
    }

    #[test]
    fn test_response_status_parsing() {
        // Test 200 OK
        let response_200 = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<body>";
        let resp = HttpResponse::from_bytes(response_200.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.reason_phrase, "OK");
        assert_eq!(resp.version, "HTTP/1.1");

        // Test 404 Not Found
        let response_404 = "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\n\r\nNot Found";
        let resp = HttpResponse::from_bytes(response_404.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 404);
        assert_eq!(resp.reason_phrase, "Not Found");

        // Test 301 Moved Permanently
        let response_301 = "HTTP/1.1 301 Moved Permanently\r\nLocation: /new-url\r\n\r\n";
        let resp = HttpResponse::from_bytes(response_301.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 301);
        assert_eq!(resp.reason_phrase, "Moved Permanently");
    }

    #[test]
    fn test_response_multi_value_headers() {
        let response = "HTTP/1.1 200 OK\r\n\
                       Set-Cookie: session=abc123; Path=/\r\n\
                       Set-Cookie: user=john; Domain=example.com\r\n\
                       Content-Type: text/html\r\n\r\n<body>";

        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        // Test getting all Set-Cookie values
        let all_cookies = resp.header_all("Set-Cookie");
        assert_eq!(all_cookies.len(), 2);
        assert!(all_cookies.contains(&"session=abc123; Path=/".to_string()));
        assert!(all_cookies.contains(&"user=john; Domain=example.com".to_string()));

        // Test getting the first value
        let first_cookie = resp.header("Set-Cookie").unwrap();
        assert_eq!(first_cookie, "session=abc123; Path=/");
    }

    #[test]
    fn test_response_content_length() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        assert_eq!(resp.content_length(), Some(1024));

        // No Content-Length
        let response_no_cl = "HTTP/1.1 200 OK\r\n\r\n";
        let resp_no_cl = HttpResponse::from_bytes(response_no_cl.as_bytes()).unwrap();
        assert_eq!(resp_no_cl.content_length(), None);
    }

    #[test]
    fn test_response_is_redirect() {
        // Redirect status codes
        let redirect_resp = HttpResponse::from_bytes(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: /new\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert!(redirect_resp.is_redirect());

        let redirect_302 =
            HttpResponse::from_bytes("HTTP/1.1 302 Found\r\n\r\n".as_bytes()).unwrap();
        assert!(redirect_302.is_redirect());

        // Non-redirect status codes
        let ok_resp = HttpResponse::from_bytes("HTTP/1.1 200 OK\r\n\r\n".as_bytes()).unwrap();
        assert!(!ok_resp.is_redirect());

        let error_resp =
            HttpResponse::from_bytes("HTTP/1.1 500 Internal Server Error\r\n\r\n".as_bytes())
                .unwrap();
        assert!(!error_resp.is_redirect());
    }

    #[test]
    fn test_response_location() {
        let response =
            "HTTP/1.1 301 Moved Permanently\r\nLocation: https://example.com/new-page\r\n\r\n";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        let location = resp.location().unwrap();
        assert_eq!(location.as_str(), "https://example.com/new-page");
    }

    #[test]
    fn test_response_body_parsing() {
        let response =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"success\"}";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        assert!(resp.body.is_some());
        assert_eq!(resp.body.unwrap(), b"{\"status\":\"success\"}");

        // No body
        let response_no_body = "HTTP/1.1 204 No Content\r\n\r\n";
        let resp_no_body = HttpResponse::from_bytes(response_no_body.as_bytes()).unwrap();
        assert!(resp_no_body.body.is_none());
    }

    #[test]
    fn test_basic_auth_header_generation() {
        // Test basic Base64 encoding
        let auth = basic_auth("user", "pass");
        assert_eq!(auth, "Basic dXNlcjpwYXNz");

        // Test special characters
        let auth_special = basic_auth("admin@email.com", "p@ssw0rd!");
        // Verify format is correct
        assert!(auth_special.starts_with("Basic "));
        // Verify it can be decoded back to original credentials
        let encoded = &auth_special["Basic ".len()..];
        let decoded = String::from_utf8(
            general_purpose::STANDARD
                .decode(encoded)
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert_eq!(decoded, "admin@email.com:p@ssw0rd!");

        // Test empty password
        let auth_empty_pass = basic_auth("user", "");
        assert!(auth_empty_pass.starts_with("Basic "));
    }

    #[test]
    fn test_bearer_token_generation() {
        let token = bearer_token("my-access-token-12345");
        assert_eq!(token, "Bearer my-access-token-12345");
    }

    #[test]
    fn test_http_method_display() {
        assert_eq!(HttpMethod::Get.to_string(), "GET");
        assert_eq!(HttpMethod::Post.to_string(), "POST");
        assert_eq!(HttpMethod::Head.to_string(), "HEAD");
        assert_eq!(HttpMethod::Put.to_string(), "PUT");
        assert_eq!(HttpMethod::Delete.to_string(), "DELETE");
    }

    #[test]
    fn test_request_builder_batch_headers() {
        let url = Url::parse("http://example.com/api").unwrap();
        let mut custom_headers = HashMap::new();
        custom_headers.insert("X-Custom-1".to_string(), "value1".to_string());
        custom_headers.insert("X-Custom-2".to_string(), "value2".to_string());

        let request = HttpRequestBuilder::new(HttpMethod::Get, url)
            .headers(custom_headers.clone())
            .build()
            .unwrap();

        assert_eq!(request.headers.get("X-Custom-1").unwrap(), "value1");
        assert_eq!(request.headers.get("X-Custom-2").unwrap(), "value2");
    }

    #[test]
    fn test_response_case_insensitive_headers() {
        let response = "HTTP/1.1 200 OK\r\n\
                       Content-Type: text/html\r\n\
                       content-length: 100\r\n\r\n";

        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        // Case-insensitive lookup
        assert!(resp.header("content-type").is_some());
        assert!(resp.header("CONTENT-TYPE").is_some());
        assert!(resp.header("Content-Length").is_some());
    }
}
