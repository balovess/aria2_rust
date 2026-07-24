//! Streaming HTTP header processor
//!
//! Incrementally parses HTTP response headers as they arrive over the network.
//! This is the Rust equivalent of the C++ `HttpHeaderProcessor`, providing:
//! - Streaming/incremental parsing (data may arrive in arbitrary TCP chunks)
//! - Obsolete line folding (obs-fold, RFC 7230 section 3.2.4)
//! - Case-insensitive header name lookup
//! - Transfer-Encoding overrides Content-Length per RFC 7230 section 3.3.2
//!
//! # Example
//!
//! ```rust,ignore
//! use aria2_core::http::header_processor::HttpHeaderProcessor;
//!
//! let mut proc = HttpHeaderProcessor::new();
//!
//! // First chunk arrives (incomplete)
//! let state = proc.feed(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n");
//! assert!(!state.is_complete());
//!
//! // Second chunk with end-of-headers marker
//! let state = proc.feed(b"Content-Length: 42\r\n\r\n");
//! assert!(state.is_complete());
//!
//! let head = proc.get_result().unwrap();
//! assert_eq!(head.status_code, 200);
//! ```

use tracing::debug;

use crate::error::{Aria2Error, Result};

/// Maximum allowed size for a single header field name (bytes).
const MAX_FIELD_NAME_LEN: usize = 1024;
/// Maximum allowed size for a single header field value (bytes).
const MAX_FIELD_VALUE_LEN: usize = 8192;
/// Maximum total header block size (bytes). Prevents OOM from malicious servers.
const MAX_HEADER_SIZE: usize = 65536;

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
        self.header("content-length").and_then(|v| v.parse::<u64>().ok())
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

// ---------------------------------------------------------------------------
// HttpHeaderProcessor
// ---------------------------------------------------------------------------

/// Streaming HTTP header processor.
///
/// Accepts data incrementally via [`feed`](Self::feed) and parses HTTP response
/// headers as they arrive over the network. Once the `\r\n\r\n` terminator is
/// detected, the parsed result can be obtained via
/// [`get_result`](Self::get_result).
pub struct HttpHeaderProcessor {
    /// Internal buffer for accumulating incoming bytes (header data only).
    buf: Vec<u8>,
    /// Current parse state.
    state: HttpHeaderParseState,
    /// Number of bytes from the last `feed()` call consumed as header data.
    last_bytes_processed: usize,
}

impl HttpHeaderProcessor {
    /// Create a new processor in the initial state.
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(512),
            state: HttpHeaderParseState::ParsingStatusLine,
            last_bytes_processed: 0,
        }
    }

    /// Feed incremental data to the processor.
    ///
    /// Returns the current parse state. When the state becomes
    /// [`Complete`](HttpHeaderParseState::Complete), use
    /// [`last_bytes_processed`](Self::last_bytes_processed) to determine how
    /// many bytes of the supplied `data` belong to the header section; the
    /// remaining bytes are body data that should be handled separately.
    ///
    /// Calling `feed` after `Complete` or `Error` is a no-op.
    pub fn feed(&mut self, data: &[u8]) -> &HttpHeaderParseState {
        self.last_bytes_processed = 0;

        // No-op if already terminal
        if !matches!(
            self.state,
            HttpHeaderParseState::ParsingStatusLine | HttpHeaderParseState::ParsingHeaders
        ) {
            return &self.state;
        }

        let prev_len = self.buf.len();
        self.buf.extend_from_slice(data);

        // Check for header terminator \r\n\r\n
        if let Some(pos) = self.find_terminator() {
            let header_end = pos + 4; // includes the \r\n\r\n
            self.buf.truncate(header_end); // discard body bytes
            self.last_bytes_processed = header_end - prev_len;
            self.state = HttpHeaderParseState::Complete;
            debug!(header_bytes = header_end, "HTTP headers complete");
        } else {
            // All supplied bytes are header bytes so far
            self.last_bytes_processed = data.len();

            // Advance state if we have at least the status line
            if matches!(self.state, HttpHeaderParseState::ParsingStatusLine)
                && Self::find_crlf(&self.buf, 0).is_some()
            {
                self.state = HttpHeaderParseState::ParsingHeaders;
            }

            // Guard against unbounded buffer growth
            if self.buf.len() > MAX_HEADER_SIZE {
                self.state = HttpHeaderParseState::Error(
                    "Too large HTTP header block".to_string(),
                );
                debug!(buf_len = self.buf.len(), "Header block exceeds size limit");
            }
        }

        &self.state
    }

    /// Number of bytes from the last `feed()` call that belong to the header
    /// section. Bytes beyond this count in the supplied data are body data.
    pub fn last_bytes_processed(&self) -> usize {
        self.last_bytes_processed
    }

    /// Parse the accumulated header data into an [`HttpResponseHead`].
    ///
    /// Only valid when the parse state is `Complete`. Returns an error if
    /// the header data is malformed.
    pub fn get_result(&self) -> Result<HttpResponseHead> {
        if !matches!(self.state, HttpHeaderParseState::Complete) {
            return Err(Aria2Error::Parse(
                "Headers not yet complete".to_string(),
            ));
        }

        let header_str = std::str::from_utf8(&self.buf)
            .map_err(|e| Aria2Error::Parse(format!("Invalid UTF-8 in HTTP headers: {}", e)))?;

        Self::parse_header_block(header_str)
    }

    /// Get the raw header bytes as a string (includes the trailing `\r\n\r\n`).
    pub fn get_header_string(&self) -> String {
        String::from_utf8_lossy(&self.buf).to_string()
    }

    /// Reset to initial state for reuse on a new connection.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.state = HttpHeaderParseState::ParsingStatusLine;
        self.last_bytes_processed = 0;
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Find the `\r\n\r\n` terminator in the buffer.
    fn find_terminator(&self) -> Option<usize> {
        self.buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
    }

    /// Find the next `\r\n` in `data` starting from `from`.
    fn find_crlf(data: &[u8], from: usize) -> Option<usize> {
        data[from..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .map(|p| from + p)
    }

    /// Parse a complete header block into `HttpResponseHead`.
    fn parse_header_block(header_str: &str) -> Result<HttpResponseHead> {
        // Strip the trailing \r\n\r\n terminator
        let header_str = header_str
            .strip_suffix("\r\n\r\n")
            .unwrap_or(header_str);

        let mut lines = header_str.lines();

        // --- Status line ---
        let status_line = lines.next().ok_or_else(|| {
            Aria2Error::Parse("Empty HTTP response".to_string())
        })?;
        let (http_version, status_code, reason_phrase) =
            Self::parse_status_line(status_line)?;

        // --- Headers with obs-fold support ---
        let headers = Self::parse_headers(lines)?;

        // --- RFC 7230 section 3.3.2 ---
        // If Transfer-Encoding is present, remove Content-Length and Content-Range.
        let has_te = headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"));
        let filtered = if has_te {
            headers
                .into_iter()
                .filter(|(k, _)| {
                    !k.eq_ignore_ascii_case("content-length")
                        && !k.eq_ignore_ascii_case("content-range")
                })
                .collect()
        } else {
            headers
        };

        Ok(HttpResponseHead {
            http_version,
            status_code,
            reason_phrase,
            headers: filtered,
        })
    }

    /// Parse the HTTP status line: `HTTP/x.x status_code reason_phrase`
    fn parse_status_line(line: &str) -> Result<(String, u16, String)> {
        let mut parts = line.splitn(3, ' ');

        let version = parts.next().ok_or_else(|| {
            Aria2Error::Parse("Bad Status-Line: missing HTTP-version".to_string())
        })?;

        if !version.starts_with("HTTP/") {
            return Err(Aria2Error::Parse(
                "Bad Status-Line: missing HTTP-version".to_string(),
            ));
        }

        let code_str = parts.next().ok_or_else(|| {
            Aria2Error::Parse("Bad Status-Line: missing status-code".to_string())
        })?;

        let status_code: u16 = code_str.parse().map_err(|_| {
            Aria2Error::Parse("Bad status code: invalid status-code".to_string())
        })?;

        if status_code < 100 {
            return Err(Aria2Error::Parse(
                "Bad status code: status-code < 100".to_string(),
            ));
        }

        let reason_phrase = parts.next().unwrap_or("").to_string();

        Ok((version.to_string(), status_code, reason_phrase))
    }

    /// Parse header lines with obs-fold continuation support.
    fn parse_headers<'a, I: Iterator<Item = &'a str>>(
        lines: I,
    ) -> Result<Vec<(String, String)>> {
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current_value = String::new();

        for line in lines {
            if line.is_empty() {
                continue;
            }

            // obs-fold: line starting with SP or HT is a continuation
            if line.starts_with(' ') || line.starts_with('\t') {
                if current_name.is_none() {
                    return Err(Aria2Error::Parse(
                        "Bad HTTP header: field name starts with LWS".to_string(),
                    ));
                }
                if !current_value.is_empty() {
                    current_value.push(' ');
                }
                current_value.push_str(line.trim_start());
                continue;
            }

            // Flush previous header before starting a new one
            if let Some(name) = current_name.take() {
                let value = current_value.trim().to_string();
                Self::validate_field_sizes(&name, &value)?;
                headers.push((name, value));
            }
            current_value.clear();

            // Parse "name: value"
            match line.find(':') {
                Some(0) => {
                    return Err(Aria2Error::Parse(
                        "Bad HTTP header: field name starts with ':'".to_string(),
                    ));
                }
                Some(pos) => {
                    let name = line[..pos].to_lowercase();
                    let value = line[pos + 1..].trim_start().to_string();
                    current_name = Some(name);
                    current_value = value;
                }
                None => {
                    return Err(Aria2Error::Parse(
                        "Bad HTTP header: missing ':'".to_string(),
                    ));
                }
            }
        }

        // Flush last header
        if let Some(name) = current_name.take() {
            let value = current_value.trim().to_string();
            Self::validate_field_sizes(&name, &value)?;
            headers.push((name, value));
        }

        Ok(headers)
    }

    /// Validate header field sizes (matches C++ limits).
    fn validate_field_sizes(name: &str, value: &str) -> Result<()> {
        if name.len() > MAX_FIELD_NAME_LEN {
            return Err(Aria2Error::Parse(
                "Too large HTTP header: field name exceeds limit".to_string(),
            ));
        }
        if value.len() > MAX_FIELD_VALUE_LEN {
            return Err(Aria2Error::Parse(
                "Too large HTTP header: field value exceeds limit".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for HttpHeaderProcessor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_headers_complete_in_one_feed() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 42\r\n\r\n";
        let state = proc.feed(data);
        assert!(state.is_complete());

        let head = proc.get_result().unwrap();
        assert_eq!(head.http_version, "HTTP/1.1");
        assert_eq!(head.status_code, 200);
        assert_eq!(head.reason_phrase, "OK");
        assert_eq!(head.header("content-type"), Some("text/html"));
        assert_eq!(head.content_length(), Some(42));
    }

    #[test]
    fn test_incremental_feeding() {
        let mut proc = HttpHeaderProcessor::new();

        // First chunk: status line only
        let state = proc.feed(b"HTTP/1.1 302 Found\r\n");
        assert!(!state.is_complete());
        assert_eq!(proc.last_bytes_processed(), 20);

        // Second chunk: one header
        let state = proc.feed(b"Location: /new\r\n");
        assert!(!state.is_complete());
        assert_eq!(proc.last_bytes_processed(), 16);

        // Third chunk: terminator
        let state = proc.feed(b"\r\n");
        assert!(state.is_complete());
        assert_eq!(proc.last_bytes_processed(), 2);

        let head = proc.get_result().unwrap();
        assert_eq!(head.status_code, 302);
        assert_eq!(head.reason_phrase, "Found");
        assert_eq!(head.header("location"), Some("/new"));
    }

    #[test]
    fn test_body_bytes_after_headers() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\n\r\n<body data here>";
        let state = proc.feed(data);
        assert!(state.is_complete());

        // Only 19 bytes are header bytes (HTTP/1.1 200 OK\r\n\r\n)
        assert_eq!(proc.last_bytes_processed(), 19);

        let head = proc.get_result().unwrap();
        assert_eq!(head.status_code, 200);
    }

    #[test]
    fn test_body_bytes_across_feeds() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n");
        // This feed completes headers AND includes body bytes
        let state = proc.feed(b"\r\nhello");
        assert!(state.is_complete());

        // \r\n (terminator) = 2 bytes from this feed are header bytes
        assert_eq!(proc.last_bytes_processed(), 2);

        // "hello" (5 bytes) are body bytes, not in the header result
        let head = proc.get_result().unwrap();
        assert_eq!(head.header("content-length"), Some("5"));
    }

    #[test]
    fn test_obs_fold_multiline_header() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nX-Custom: hello\r\n world\r\n\r\n";
        let state = proc.feed(data);
        assert!(state.is_complete());

        let head = proc.get_result().unwrap();
        // obs-fold: " world" appended to "hello" with space separator
        assert_eq!(head.header("x-custom"), Some("hello world"));
    }

    #[test]
    fn test_obs_fold_tab() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nX-Folded: line1\r\n\tline2\r\n\r\n";
        let state = proc.feed(data);
        assert!(state.is_complete());

        let head = proc.get_result().unwrap();
        assert_eq!(head.header("x-folded"), Some("line1 line2"));
    }

    #[test]
    fn test_obs_fold_without_previous_header_is_error() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\n continuation-without-name\r\n\r\n";
        proc.feed(data);

        // Error is detected during parse, not during feed
        let result = proc.get_result();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("LWS"));
    }

    #[test]
    fn test_multiple_same_name_headers() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n";
        proc.feed(data);

        let head = proc.get_result().unwrap();
        let cookies = head.header_all("set-cookie");
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0], "a=1");
        assert_eq!(cookies[1], "b=2");
    }

    #[test]
    fn test_case_insensitive_lookup() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n";
        proc.feed(data);

        let head = proc.get_result().unwrap();
        assert_eq!(head.header("content-type"), Some("text/html"));
        assert_eq!(head.header("CONTENT-TYPE"), Some("text/html"));
        assert_eq!(head.header("Content-Type"), Some("text/html"));
    }

    #[test]
    fn test_transfer_encoding_overrides_content_length() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 999\r\nContent-Range: bytes 0-499/1000\r\n\r\n";
        proc.feed(data);

        let head = proc.get_result().unwrap();
        assert!(head.has_transfer_encoding());
        // Content-Length and Content-Range must be removed per RFC 7230
        assert_eq!(head.header("content-length"), None);
        assert_eq!(head.header("content-range"), None);
    }

    #[test]
    fn test_malformed_status_line_missing_version() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"GARBAGE 200 OK\r\n\r\n";
        proc.feed(data);

        let result = proc.get_result();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("HTTP-version"));
    }

    #[test]
    fn test_malformed_status_line_missing_code() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1\r\n\r\n";
        proc.feed(data);

        let result = proc.get_result();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("status-code"));
    }

    #[test]
    fn test_malformed_status_line_invalid_code() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 abc OK\r\n\r\n";
        proc.feed(data);

        let result = proc.get_result();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("status-code"));
    }

    #[test]
    fn test_malformed_status_code_below_100() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 099 OK\r\n\r\n";
        proc.feed(data);

        let result = proc.get_result();
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_end_of_headers() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n";
        let state = proc.feed(data);
        assert!(!state.is_complete());
    }

    #[test]
    fn test_header_name_starts_with_colon() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\n: bad-name\r\n\r\n";
        proc.feed(data);

        let result = proc.get_result();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("':'"));
    }

    #[test]
    fn test_header_missing_colon() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nNoColonHere\r\n\r\n";
        proc.feed(data);

        let result = proc.get_result();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("':'"));
    }

    #[test]
    fn test_reason_phrase_with_spaces() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 500 Internal Server Error\r\n\r\n";
        proc.feed(data);

        let head = proc.get_result().unwrap();
        assert_eq!(head.status_code, 500);
        assert_eq!(head.reason_phrase, "Internal Server Error");
    }

    #[test]
    fn test_no_reason_phrase() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200\r\n\r\n";
        proc.feed(data);

        let head = proc.get_result().unwrap();
        assert_eq!(head.status_code, 200);
        assert_eq!(head.reason_phrase, "");
    }

    #[test]
    fn test_oversized_header_block() {
        let mut proc = HttpHeaderProcessor::new();
        // Feed data until buffer exceeds MAX_HEADER_SIZE
        let big_chunk = vec![b'X'; MAX_HEADER_SIZE + 1];
        let state = proc.feed(&big_chunk);
        assert!(state.is_error());
    }

    #[test]
    fn test_clear_resets_state() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\n\r\n");
        assert!(proc.get_result().is_ok());

        proc.clear();
        assert_eq!(proc.state, HttpHeaderParseState::ParsingStatusLine);
        assert_eq!(proc.last_bytes_processed(), 0);

        // Can process a new response
        proc.feed(b"HTTP/1.1 404 Not Found\r\n\r\n");
        let head = proc.get_result().unwrap();
        assert_eq!(head.status_code, 404);
    }

    #[test]
    fn test_feed_after_complete_is_noop() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\n\r\n");
        assert!(proc.state.is_complete());

        // Feed more data after completion — should not change state
        let state = proc.feed(b"extra body data");
        assert!(state.is_complete());
        assert_eq!(proc.last_bytes_processed(), 0);
    }

    #[test]
    fn test_get_header_string() {
        let mut proc = HttpHeaderProcessor::new();
        let data = b"HTTP/1.1 200 OK\r\nServer: test\r\n\r\n";
        proc.feed(data);

        let header_str = proc.get_header_string();
        assert!(header_str.starts_with("HTTP/1.1 200 OK"));
        assert!(header_str.contains("Server: test"));
        assert!(header_str.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_split_terminator_across_feeds() {
        let mut proc = HttpHeaderProcessor::new();
        // Feed ends with \r (first half of \r\n in the terminator)
        proc.feed(b"HTTP/1.1 200 OK\r\n\r");
        assert!(!proc.state.is_complete());
        // Complete the \r\n\r\n terminator
        let state = proc.feed(b"\n");
        assert!(state.is_complete());
    }

    #[test]
    fn test_http1_0_response() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");

        let head = proc.get_result().unwrap();
        assert_eq!(head.http_version, "HTTP/1.0");
        assert_eq!(head.status_code, 200);
    }

    #[test]
    fn test_iter_headers() {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\n\r\n");

        let head = proc.get_result().unwrap();
        let pairs: Vec<_> = head.iter_headers().collect();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("a", "1"));
        assert_eq!(pairs[1], ("b", "2"));
    }
}
