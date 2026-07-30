//! Streaming HTTP header processor implementation.

use tracing::debug;

use crate::error::{Aria2Error, Result};

use super::types::{
    HttpHeaderParseState, HttpResponseHead, MAX_FIELD_NAME_LEN, MAX_FIELD_VALUE_LEN,
    MAX_HEADER_SIZE,
};

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
    pub(crate) state: HttpHeaderParseState,
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
                self.state = HttpHeaderParseState::Error("Too large HTTP header block".to_string());
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
            return Err(Aria2Error::Parse("Headers not yet complete".to_string()));
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
        self.buf.windows(4).position(|w| w == b"\r\n\r\n")
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
        let status_line = lines
            .next()
            .ok_or_else(|| Aria2Error::Parse("Empty HTTP response".to_string()))?;
        let (http_version, status_code, reason_phrase) = Self::parse_status_line(status_line)?;

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

        Ok(HttpResponseHead::new(
            http_version,
            status_code,
            reason_phrase,
            filtered,
        ))
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

        let code_str = parts
            .next()
            .ok_or_else(|| Aria2Error::Parse("Bad Status-Line: missing status-code".to_string()))?;

        let status_code: u16 = code_str
            .parse()
            .map_err(|_| Aria2Error::Parse("Bad status code: invalid status-code".to_string()))?;

        if status_code < 100 {
            return Err(Aria2Error::Parse(
                "Bad status code: status-code < 100".to_string(),
            ));
        }

        let reason_phrase = parts.next().unwrap_or("").to_string();

        Ok((version.to_string(), status_code, reason_phrase))
    }

    /// Parse header lines with obs-fold continuation support.
    fn parse_headers<'a, I: Iterator<Item = &'a str>>(lines: I) -> Result<Vec<(String, String)>> {
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
