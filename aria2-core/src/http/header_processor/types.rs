//! HTTP header types: parse state, response head, and size constants.

/// Maximum allowed size for a single header field name (bytes).
pub const MAX_FIELD_NAME_LEN: usize = 1024;
/// Maximum allowed size for a single header field value (bytes).
pub const MAX_FIELD_VALUE_LEN: usize = 8192;
/// Maximum total header block size (bytes). Prevents OOM from malicious servers.
pub const MAX_HEADER_SIZE: usize = 65536;

// ---------------------------------------------------------------------------
// HttpHeaderParseState
// ---------------------------------------------------------------------------

/// Parse state for the streaming HTTP header processor.
#[derive(Debug, Clone, PartialEq)]
pub enum HttpHeaderParseState {
    /// Waiting for the status line to arrive.
    ParsingStatusLine,
    /// Status line received; collecting header lines.
    ParsingHeaders,
    /// End-of-headers marker (`\r\n\r\n`) received; header data is complete.
    Complete,
    /// Malformed input detected; parsing cannot continue.
    Error(String),
}

impl HttpHeaderParseState {
    /// Returns `true` if headers are fully parsed.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Returns `true` if the processor is in an error state.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

// ---------------------------------------------------------------------------
// HttpResponseHead
// ---------------------------------------------------------------------------

/// Parsed HTTP response head (status line + headers, no body).
///
/// Header names are stored in lowercase for efficient case-insensitive lookup.
/// Duplicate header names are preserved (important for `Set-Cookie` etc.).
#[derive(Debug, Clone)]
pub struct HttpResponseHead {
    /// HTTP version string (e.g., `"HTTP/1.1"`)
    pub http_version: String,
    /// HTTP status code (e.g., 200, 404)
    pub status_code: u16,
    /// Reason phrase (e.g., `"OK"`, `"Not Found"`)
    pub reason_phrase: String,
    /// Ordered header name-value pairs; names are lowercase.
    headers: Vec<(String, String)>,
}

impl HttpResponseHead {
    /// Look up the first value for a header name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Look up all values for a header name (case-insensitive).
    /// Useful for headers like `Set-Cookie` that may appear multiple times.
    pub fn header_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Parse `Content-Length` header value as `u64`.
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")
            .and_then(|v| v.parse::<u64>().ok())
    }

    /// Check whether `Transfer-Encoding` header is present.
    pub fn has_transfer_encoding(&self) -> bool {
        self.header("transfer-encoding").is_some()
    }

    /// Returns an iterator over all header name-value pairs.
    pub fn iter_headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}
