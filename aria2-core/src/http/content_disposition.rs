//! RFC 6266 Content-Disposition header parsing.
//!
//! Parses `Content-Disposition` headers as defined in RFC 6266, extracting the
//! disposition type and filename parameters. Mirrors the C++ state-machine
//! parser in `aria2_original/src/util.cc` (`parse_content_disposition`) while
//! providing a more ergonomic Rust API.
//!
//! # Key rules (from RFC 6266 and the C++ implementation)
//!
//! - `filename*=charset'language'encoded_value` takes priority over `filename=`.
//! - `filename*=UTF-8''value` uses percent-encoding.
//! - `filename="quoted string"` strips surrounding quotes.
//! - `filename=unquoted` is also valid.
//! - Non-ASCII bytes in `filename=` are interpreted as UTF-8 (matching C++
//!   `defaultUTF8` mode); ISO-8859-1 bytes are converted to UTF-8 when the
//!   charset is explicitly `iso-8859-1`.
//! - Directory-traversal filenames (`..`, `/`, `\`, etc.) are rejected.

use tracing::trace;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of parsing a `Content-Disposition` header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentDispositionResult {
    /// The disposition type (e.g., `"attachment"`, `"inline"`, `"form-data"`).
    pub disposition_type: String,
    /// The decoded filename, preferring `filename*` per RFC 6266.
    /// `None` if no filename parameter is present or the value is invalid.
    pub filename: Option<String>,
    /// The ASCII-only filename from the `filename=` parameter (not `filename*`).
    /// `None` if `filename=` is absent or its value is empty/invalid.
    pub filename_ascii: Option<String>,
}

// ---------------------------------------------------------------------------
// Character classification helpers (matching C++ RFC helpers)
// ---------------------------------------------------------------------------

/// Characters allowed in an RFC 2616 token.
#[inline]
fn is_rfc2616_http_token(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Characters allowed in an RFC 2978 MIME charset name.
#[inline]
fn is_rfc2978_mime_charset(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'+'
                | b'-'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'}'
                | b'~'
        )
}

/// Characters allowed in an RFC 5987 attr-char (token minus `*`, `'`, `%`).
#[inline]
fn is_rfc5987_attr_char(c: u8) -> bool {
    is_rfc2616_http_token(c) && !matches!(c, b'*' | b'\'' | b'%')
}

/// Whether a byte is linear whitespace (SP / HTAB).
#[inline]
fn is_lws(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// Whether a byte is in ISO/IEC 8859-1 printable range.
///
/// TODO: Should be used in `ValueChars` state when `ext_charset == Iso8859p1`
/// to accept 0xA0-0xFF bytes per the C++ implementation. Currently the
/// `ValueChars` state only accepts `is_rfc5987_attr_char` bytes.
#[allow(dead_code)]
#[inline]
fn is_iso8859p1(c: u8) -> bool {
    (0x20..=0x7e).contains(&c) || c >= 0xa0
}

// ---------------------------------------------------------------------------
// Charset enum for filename* values
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charset {
    Unknown,
    Utf8,
    Iso8859p1,
}

// ---------------------------------------------------------------------------
// Parser state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    BeforeDispositionType,
    DispositionType,
    AfterDispositionType,
    BeforeParmName,
    ParmName,
    AfterParmName,
    BeforeValue,
    QuotedString,
    Token,
    AfterValue,
    BeforeExtValue,
    Charset,
    Language,
    ValueChars,
    ValueCharsPctEncoded1,
    ValueCharsPctEncoded2,
}

/// Flags tracking which filename variants have been seen.
#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    filename_found: bool,
    ext_filename_found: bool,
}

// ---------------------------------------------------------------------------
// UTF-8 validation helper
// ---------------------------------------------------------------------------

/// Validate a byte sequence as UTF-8, returning the decoded String or None.
/// This replaces the C++ DFA-based `utf8dfa` validation.
#[inline]
fn validate_utf8(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(|s| s.to_owned())
}

// ---------------------------------------------------------------------------
// ISO-8859-1 to UTF-8 conversion
// ---------------------------------------------------------------------------

/// Convert ISO-8859-1 bytes to a UTF-8 String.
/// Returns `None` if the input contains bytes in the 0x80..0x9F range
/// (C1 control characters not valid in ISO-8859-1 per the C++ implementation).
fn iso8859p1_to_utf8(src: &[u8]) -> Option<String> {
    let mut out = String::with_capacity(src.len() * 2);
    for &c in src {
        if c >= 0xa0 {
            // ISO-8859-1 bytes 0xA0..0xFF map 1:1 to Unicode codepoints U+00A0..U+00FF.
            // Rust String::push handles the UTF-8 encoding automatically.
            out.push(c as char);
        } else if (0x80..=0x9f).contains(&c) {
            // C1 control characters — invalid in ISO-8859-1 per C++ impl
            return None;
        } else {
            out.push(c as char);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Directory-traversal detection (matching C++ `detectDirTraversal`)
// ---------------------------------------------------------------------------

/// Check whether a filename contains directory-traversal components or
/// path separators, making it unsafe for use as a download filename.
fn is_dir_traversal(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Reject control characters
    for c in s.bytes() {
        if c <= 0x1f || c == 0x7f {
            return true;
        }
    }
    // Reject path separators and traversal patterns
    s == "."
        || s == ".."
        || s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.contains("/../")
        || s.contains("/./")
        || s.ends_with('/')
        || s.ends_with("/.")
        || s.ends_with("/..")
        // Also reject backslash (Windows path separator)
        || s.contains('\\')
}

// ---------------------------------------------------------------------------
// Core parser
// ---------------------------------------------------------------------------

/// Parsed raw filename data from the state machine.
struct RawFilename {
    /// Bytes collected for `filename=` (unquoted, raw bytes).
    filename_bytes: Vec<u8>,
    /// Bytes collected for `filename*=` (decoded, raw bytes).
    ext_filename_bytes: Vec<u8>,
    /// Charset detected in `filename*=...` parameter.
    ext_charset: Charset,
}

/// Parse a Content-Disposition header value and return the raw filename data
/// along with the disposition type.
///
/// This mirrors the C++ `parse_content_disposition` state machine but
/// returns structured data instead of writing into a fixed-size buffer.
fn parse_raw(header_value: &str) -> Option<(String, RawFilename)> {
    let input = header_value.as_bytes();
    let mut state = ParseState::BeforeDispositionType;

    let mut mark_first: usize = 0;
    let mut mark_last: usize = 0;

    // Separate tracking for disposition type span (mark_first is reused for parm names)
    let mut disposition_type_start: usize = 0;

    let mut flags = Flags::default();

    let mut disposition_type_end: usize = 0;
    let mut disposition_type_started = false;

    let mut filename_bytes: Vec<u8> = Vec::new();
    let mut ext_filename_bytes: Vec<u8> = Vec::new();
    let mut ext_charset = Charset::Unknown;

    let mut in_file_parm: bool = false;

    // Quoted-string backslash tracking
    let mut quoted_seen: bool = false;

    // Percent-encoding accumulator
    let mut pctval: u8 = 0;

    // Charset span for filename*
    let mut charset_start: usize = 0;

    // Which filename buffer are we currently writing into?
    // None means we are not inside a filename parameter.
    enum FileTarget {
        None,
        Filename,
        ExtFilename,
    }
    let mut file_target = FileTarget::None;

    let mut i = 0;
    while i < input.len() {
        let c = input[i];
        match state {
            ParseState::BeforeDispositionType => {
                if is_rfc2616_http_token(c) {
                    if !disposition_type_started {
                        disposition_type_started = true;
                        disposition_type_start = i;
                    }
                    state = ParseState::DispositionType;
                } else if !is_lws(c) {
                    trace!(
                        "parse_raw: unexpected byte 0x{:02x} in BeforeDispositionType",
                        c
                    );
                    return None;
                }
            }
            ParseState::DispositionType | ParseState::AfterDispositionType => {
                if c == b';' {
                    if state == ParseState::DispositionType {
                        disposition_type_end = i;
                    }
                    state = ParseState::BeforeParmName;
                } else if is_lws(c) {
                    if state == ParseState::DispositionType {
                        disposition_type_end = i;
                    }
                    state = ParseState::AfterDispositionType;
                } else if state == ParseState::AfterDispositionType || !is_rfc2616_http_token(c) {
                    trace!("parse_raw: unexpected byte 0x{:02x} in DispositionType", c);
                    return None;
                }
                // Otherwise still parsing disposition type token
            }
            ParseState::BeforeParmName => {
                if is_rfc2616_http_token(c) {
                    mark_first = i;
                    state = ParseState::ParmName;
                } else if !is_lws(c) {
                    trace!("parse_raw: unexpected byte 0x{:02x} in BeforeParmName", c);
                    return None;
                }
            }
            ParseState::ParmName | ParseState::AfterParmName => {
                if c == b'=' {
                    if state == ParseState::ParmName {
                        mark_last = i;
                    }
                    let parm_name = &input[mark_first..mark_last];

                    // Reset file target for each new parameter
                    file_target = FileTarget::None;
                    in_file_parm = false;

                    if parm_name.eq_ignore_ascii_case(b"filename*") {
                        if flags.ext_filename_found {
                            trace!("parse_raw: duplicate filename* parameter");
                            return None;
                        }
                        in_file_parm = true;
                        file_target = FileTarget::ExtFilename;
                        ext_filename_bytes.clear();
                        state = ParseState::BeforeExtValue;
                    } else if parm_name.eq_ignore_ascii_case(b"filename") {
                        if flags.filename_found {
                            trace!("parse_raw: duplicate filename parameter");
                            return None;
                        }
                        // Always collect filename= bytes even when filename* was
                        // already found, so that filename_ascii is available as a
                        // fallback per RFC 6266.
                        in_file_parm = true;
                        file_target = FileTarget::Filename;
                        filename_bytes.clear();
                        state = ParseState::BeforeValue;
                    } else if parm_name.last() == Some(&b'*') {
                        state = ParseState::BeforeExtValue;
                    } else {
                        state = ParseState::BeforeValue;
                    }
                } else if is_lws(c) {
                    mark_last = i;
                    state = ParseState::AfterParmName;
                } else if state == ParseState::AfterParmName || !is_rfc2616_http_token(c) {
                    trace!("parse_raw: unexpected byte 0x{:02x} in ParmName", c);
                    return None;
                }
            }
            ParseState::BeforeValue => {
                if c == b'"' {
                    quoted_seen = false;
                    state = ParseState::QuotedString;
                } else if is_rfc2616_http_token(c) {
                    match file_target {
                        FileTarget::Filename => filename_bytes.push(c),
                        FileTarget::ExtFilename => ext_filename_bytes.push(c),
                        FileTarget::None => {}
                    }
                    state = ParseState::Token;
                } else if !is_lws(c) {
                    trace!("parse_raw: unexpected byte 0x{:02x} in BeforeValue", c);
                    return None;
                }
            }
            ParseState::QuotedString => {
                if c == b'\\' && !quoted_seen {
                    quoted_seen = true;
                } else if c == b'"' && !quoted_seen {
                    // End of quoted string
                    match file_target {
                        FileTarget::Filename => flags.filename_found = true,
                        FileTarget::ExtFilename => flags.ext_filename_found = true,
                        FileTarget::None => {}
                    }
                    state = ParseState::AfterValue;
                } else {
                    quoted_seen = false;
                    // Accept ISO-8859-1 chars or UTF-8 bytes in quoted strings
                    // The C++ code validates UTF-8 via DFA; we validate after collection
                    match file_target {
                        FileTarget::Filename => filename_bytes.push(c),
                        FileTarget::ExtFilename => ext_filename_bytes.push(c),
                        FileTarget::None => {}
                    }
                }
            }
            ParseState::Token => {
                if is_rfc2616_http_token(c) {
                    match file_target {
                        FileTarget::Filename => filename_bytes.push(c),
                        FileTarget::ExtFilename => ext_filename_bytes.push(c),
                        FileTarget::None => {}
                    }
                } else if c == b';' {
                    match file_target {
                        FileTarget::Filename => flags.filename_found = true,
                        FileTarget::ExtFilename => flags.ext_filename_found = true,
                        FileTarget::None => {}
                    }
                    state = ParseState::BeforeParmName;
                } else if is_lws(c) {
                    match file_target {
                        FileTarget::Filename => flags.filename_found = true,
                        FileTarget::ExtFilename => flags.ext_filename_found = true,
                        FileTarget::None => {}
                    }
                    state = ParseState::AfterValue;
                } else {
                    trace!("parse_raw: unexpected byte 0x{:02x} in Token", c);
                    return None;
                }
            }
            ParseState::AfterValue => {
                if c == b';' {
                    state = ParseState::BeforeParmName;
                } else if !is_lws(c) {
                    trace!("parse_raw: unexpected byte 0x{:02x} in AfterValue", c);
                    return None;
                }
            }
            ParseState::BeforeExtValue => {
                if c == b'\'' {
                    // Empty charset is not allowed
                    trace!("parse_raw: empty charset in filename*");
                    return None;
                } else if is_rfc2978_mime_charset(c) {
                    charset_start = i;
                    state = ParseState::Charset;
                } else if !is_lws(c) {
                    trace!("parse_raw: unexpected byte 0x{:02x} in BeforeExtValue", c);
                    return None;
                }
            }
            ParseState::Charset => {
                if c == b'\'' {
                    let charset_end = i;
                    let charset_str = &input[charset_start..charset_end];
                    if charset_str.eq_ignore_ascii_case(b"utf-8") {
                        ext_charset = Charset::Utf8;
                    } else if charset_str.eq_ignore_ascii_case(b"iso-8859-1") {
                        ext_charset = Charset::Iso8859p1;
                    } else {
                        ext_charset = Charset::Unknown;
                    }
                    state = ParseState::Language;
                } else if !is_rfc2978_mime_charset(c) {
                    trace!("parse_raw: invalid charset byte 0x{:02x}", c);
                    return None;
                }
            }
            ParseState::Language => {
                if c == b'\'' {
                    // Reset the ext filename buffer for the value part
                    if in_file_parm {
                        ext_filename_bytes.clear();
                    }
                    state = ParseState::ValueChars;
                } else if c != b'-' && !c.is_ascii_alphanumeric() {
                    trace!("parse_raw: invalid language byte 0x{:02x}", c);
                    return None;
                }
            }
            ParseState::ValueChars => {
                if is_rfc5987_attr_char(c) {
                    match file_target {
                        FileTarget::Filename => filename_bytes.push(c),
                        FileTarget::ExtFilename => ext_filename_bytes.push(c),
                        FileTarget::None => {}
                    }
                } else if c == b'%' {
                    pctval = 0;
                    state = ParseState::ValueCharsPctEncoded1;
                } else if c == b';' || is_lws(c) {
                    match file_target {
                        FileTarget::Filename => flags.filename_found = true,
                        FileTarget::ExtFilename => flags.ext_filename_found = true,
                        FileTarget::None => {}
                    }
                    if c == b';' {
                        state = ParseState::BeforeParmName;
                    } else {
                        state = ParseState::AfterValue;
                    }
                } else {
                    trace!("parse_raw: invalid value-char byte 0x{:02x}", c);
                    return None;
                }
            }
            ParseState::ValueCharsPctEncoded1 => {
                let digit = hex_digit(c);
                match digit {
                    Some(d) => {
                        pctval = d << 4;
                        state = ParseState::ValueCharsPctEncoded2;
                    }
                    None => {
                        trace!("parse_raw: expected hex digit, got 0x{:02x}", c);
                        return None;
                    }
                }
            }
            ParseState::ValueCharsPctEncoded2 => {
                let digit = hex_digit(c);
                match digit {
                    Some(d) => {
                        let byte = pctval | d;
                        match file_target {
                            FileTarget::Filename => filename_bytes.push(byte),
                            FileTarget::ExtFilename => ext_filename_bytes.push(byte),
                            FileTarget::None => {}
                        }
                        state = ParseState::ValueChars;
                    }
                    None => {
                        trace!("parse_raw: expected hex digit, got 0x{:02x}", c);
                        return None;
                    }
                }
            }
        }
        i += 1;
    }

    // Handle end-of-input terminal states
    match state {
        ParseState::BeforeDispositionType
        | ParseState::AfterDispositionType
        | ParseState::DispositionType
        | ParseState::AfterValue
        | ParseState::Token
        | ParseState::ValueChars
        | ParseState::BeforeParmName => {} // Trailing semicolons are valid
        _ => {
            trace!("parse_raw: unexpected end state {:?}", state);
            return None;
        }
    }

    // Finalize disposition type
    if !disposition_type_started {
        return None;
    }
    if disposition_type_end == 0 {
        disposition_type_end = input.len();
    }
    let disposition_type =
        std::str::from_utf8(&input[disposition_type_start..disposition_type_end])
            .ok()?
            .to_owned();

    Some((
        disposition_type,
        RawFilename {
            filename_bytes,
            ext_filename_bytes,
            ext_charset,
        },
    ))
}

/// Convert a hex ASCII digit to its numeric value.
#[inline]
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a `Content-Disposition` header value per RFC 6266.
///
/// Returns a `ContentDispositionResult` with the disposition type and
/// any filename parameters found. The `filename` field prefers `filename*`
/// over `filename=` as required by RFC 6266.
///
/// # Examples
///
/// ```
/// use aria2_core::http::content_disposition::parse_content_disposition;
///
/// let result = parse_content_disposition("attachment; filename=\"example.html\"");
/// assert_eq!(result.disposition_type, "attachment");
/// assert_eq!(result.filename.as_deref(), Some("example.html"));
/// ```
pub fn parse_content_disposition(header_value: &str) -> ContentDispositionResult {
    let (disposition_type, raw) = match parse_raw(header_value) {
        Some(pair) => pair,
        None => {
            trace!(
                header_value = %header_value,
                "parse_content_disposition: failed to parse header"
            );
            return ContentDispositionResult {
                disposition_type: String::new(),
                filename: None,
                filename_ascii: None,
            };
        }
    };

    // Decode filename= (the plain parameter)
    let filename_ascii = decode_filename_ascii(&raw.filename_bytes);

    // Decode filename*= (the extended parameter)
    let filename_ext = decode_filename_ext(&raw.ext_filename_bytes, raw.ext_charset);

    // RFC 6266: filename* takes priority over filename
    let filename = filename_ext.or(filename_ascii.clone());

    trace!(
        disposition_type = %disposition_type,
        filename = ?filename,
        filename_ascii = ?filename_ascii,
        "parse_content_disposition: parsed successfully"
    );

    ContentDispositionResult {
        disposition_type,
        filename,
        filename_ascii,
    }
}

/// Convenience function that extracts just the filename from a
/// `Content-Disposition` header value.
///
/// Returns `None` if no valid filename is found, or if the filename
/// contains directory-traversal components.
///
/// # Examples
///
/// ```
/// use aria2_core::http::content_disposition::extract_filename;
///
/// assert_eq!(
///     extract_filename("attachment; filename=\"report.pdf\""),
///     Some("report.pdf".to_owned())
/// );
/// assert_eq!(
///     extract_filename("inline"),
///     None
/// );
/// ```
pub fn extract_filename(header_value: &str) -> Option<String> {
    let result = parse_content_disposition(header_value);
    result.filename
}

// ---------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------

/// Decode the `filename=` parameter value (plain or quoted).
/// Validates bytes as UTF-8; if invalid, attempts ISO-8859-1 interpretation.
/// Rejects directory-traversal filenames.
fn decode_filename_ascii(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }

    // Try UTF-8 first (matching C++ defaultUTF8=true mode)
    if let Some(s) = validate_utf8(bytes) {
        if !is_dir_traversal(&s) {
            return Some(s);
        }
        trace!(filename = %s, "decode_filename_ascii: rejected due to directory traversal");
        return None;
    }

    // Fallback: interpret as ISO-8859-1 and convert to UTF-8
    if let Some(s) = iso8859p1_to_utf8(bytes) {
        if !is_dir_traversal(&s) {
            return Some(s);
        }
        trace!(filename = %s, "decode_filename_ascii: rejected due to directory traversal");
        return None;
    }

    trace!("decode_filename_ascii: failed to decode bytes as UTF-8 or ISO-8859-1");
    None
}

/// Decode the `filename*=` parameter value with charset awareness.
/// The bytes have already been percent-decoded by the state machine.
/// Rejects directory-traversal filenames.
fn decode_filename_ext(bytes: &[u8], charset: Charset) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }

    let decoded = match charset {
        Charset::Utf8 => validate_utf8(bytes)?,
        Charset::Iso8859p1 => iso8859p1_to_utf8(bytes)?,
        Charset::Unknown => {
            // For unknown charsets, try UTF-8 first, then ISO-8859-1
            validate_utf8(bytes).or_else(|| iso8859p1_to_utf8(bytes))?
        }
    };

    if is_dir_traversal(&decoded) {
        trace!(filename = %decoded, "decode_filename_ext: rejected due to directory traversal");
        return None;
    }

    Some(decoded)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Basic disposition type parsing --

    #[test]
    fn test_attachment_disposition() {
        let result = parse_content_disposition("attachment");
        assert_eq!(result.disposition_type, "attachment");
        assert!(result.filename.is_none());
        assert!(result.filename_ascii.is_none());
    }

    #[test]
    fn test_inline_disposition() {
        let result = parse_content_disposition("inline");
        assert_eq!(result.disposition_type, "inline");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_form_data_disposition() {
        let result = parse_content_disposition("form-data");
        assert_eq!(result.disposition_type, "form-data");
    }

    #[test]
    fn test_disposition_type_with_trailing_whitespace() {
        let result = parse_content_disposition("attachment  ");
        assert_eq!(result.disposition_type, "attachment");
    }

    #[test]
    fn test_disposition_type_with_leading_whitespace() {
        let result = parse_content_disposition("  attachment");
        assert_eq!(result.disposition_type, "attachment");
    }

    // -- filename= (unquoted) --

    #[test]
    fn test_unquoted_filename() {
        let result = parse_content_disposition("attachment; filename=example.html");
        assert_eq!(result.disposition_type, "attachment");
        assert_eq!(result.filename.as_deref(), Some("example.html"));
        assert_eq!(result.filename_ascii.as_deref(), Some("example.html"));
    }

    #[test]
    fn test_unquoted_filename_with_spaces_before_equals() {
        let result = parse_content_disposition("attachment; filename =example.html");
        assert_eq!(result.filename.as_deref(), Some("example.html"));
    }

    // -- filename= (quoted) --

    #[test]
    fn test_quoted_filename() {
        let result = parse_content_disposition("attachment; filename=\"example.html\"");
        assert_eq!(result.filename.as_deref(), Some("example.html"));
    }

    #[test]
    fn test_quoted_filename_with_spaces() {
        let result = parse_content_disposition("attachment; filename = \"example.html\"");
        assert_eq!(result.filename.as_deref(), Some("example.html"));
    }

    #[test]
    fn test_quoted_filename_with_escaped_quote() {
        // The C++ parser uses backslash-escaping in quoted strings
        let result = parse_content_disposition("attachment; filename=\"example\\\"file.html\"");
        assert_eq!(result.filename.as_deref(), Some("example\"file.html"));
    }

    // -- filename*= (RFC 5987 / RFC 6266) --

    #[test]
    fn test_ext_filename_utf8() {
        let result = parse_content_disposition(
            "attachment; filename*=UTF-8''%e3%81%93%e3%82%93%e3%81%ab%e3%81%a1%e3%81%af.txt",
        );
        assert_eq!(result.disposition_type, "attachment");
        assert_eq!(
            result.filename.as_deref(),
            Some("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}.txt")
        );
    }

    #[test]
    fn test_ext_filename_utf8_simple() {
        let result = parse_content_disposition("attachment; filename*=UTF-8''hello.txt");
        assert_eq!(result.filename.as_deref(), Some("hello.txt"));
    }

    #[test]
    fn test_ext_filename_iso_8859_1() {
        // e9 is é in ISO-8859-1
        let result = parse_content_disposition("attachment; filename*=ISO-8859-1''%e9");
        assert_eq!(result.filename.as_deref(), Some("\u{e9}"));
    }

    #[test]
    fn test_ext_filename_takes_priority_over_filename() {
        let result = parse_content_disposition(
            "attachment; filename=\"fallback.txt\"; filename*=UTF-8''preferred.txt",
        );
        assert_eq!(result.filename.as_deref(), Some("preferred.txt"));
        assert_eq!(result.filename_ascii.as_deref(), Some("fallback.txt"));
    }

    #[test]
    fn test_filename_star_before_filename_still_wins() {
        let result =
            parse_content_disposition("attachment; filename*=UTF-8''star.txt; filename=plain.txt");
        // filename* takes priority per RFC 6266
        assert_eq!(result.filename.as_deref(), Some("star.txt"));
        assert_eq!(result.filename_ascii.as_deref(), Some("plain.txt"));
    }

    #[test]
    fn test_ext_filename_with_language() {
        let result = parse_content_disposition("attachment; filename*=UTF-8'en'test.txt");
        assert_eq!(result.filename.as_deref(), Some("test.txt"));
    }

    // -- Duplicate parameter handling --

    #[test]
    fn test_duplicate_filename_is_rejected() {
        let result =
            parse_content_disposition("attachment; filename=first.txt; filename=second.txt");
        // Duplicate filename= should cause parse failure
        assert_eq!(result.disposition_type, "");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_duplicate_ext_filename_is_rejected() {
        let result = parse_content_disposition(
            "attachment; filename*=UTF-8''first.txt; filename*=UTF-8''second.txt",
        );
        assert_eq!(result.disposition_type, "");
        assert!(result.filename.is_none());
    }

    // -- Directory traversal rejection --

    #[test]
    fn test_dot_filename_rejected() {
        let result = parse_content_disposition("attachment; filename=.");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_dotdot_filename_rejected() {
        let result = parse_content_disposition("attachment; filename=..");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_absolute_path_filename_rejected() {
        let result = parse_content_disposition("attachment; filename=/etc/passwd");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_dot_slash_filename_rejected() {
        let result = parse_content_disposition("attachment; filename=./secret");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_dotdot_slash_filename_rejected() {
        let result = parse_content_disposition("attachment; filename=../secret");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_path_with_dot_component_rejected() {
        let result = parse_content_disposition("attachment; filename=dir/./file.txt");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_path_with_dotdot_component_rejected() {
        let result = parse_content_disposition("attachment; filename=dir/../file.txt");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_trailing_slash_rejected() {
        let result = parse_content_disposition("attachment; filename=dir/");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_trailing_dot_rejected() {
        let result = parse_content_disposition("attachment; filename=dir/.");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_trailing_dotdot_rejected() {
        let result = parse_content_disposition("attachment; filename=dir/..");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_backslash_filename_rejected() {
        let result = parse_content_disposition("attachment; filename=dir\\file.txt");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_control_char_filename_rejected() {
        let result = parse_content_disposition("attachment; filename=\"hello\x01world\"");
        assert!(result.filename.is_none());
    }

    // -- Invalid input --

    #[test]
    fn test_empty_input() {
        let result = parse_content_disposition("");
        assert_eq!(result.disposition_type, "");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_only_whitespace() {
        let result = parse_content_disposition("   ");
        assert_eq!(result.disposition_type, "");
        assert!(result.filename.is_none());
    }

    #[test]
    fn test_invalid_char_in_disposition_type() {
        let result = parse_content_disposition("attach@ment");
        assert_eq!(result.disposition_type, "");
    }

    // -- extract_filename convenience --

    #[test]
    fn test_extract_filename_basic() {
        assert_eq!(
            extract_filename("attachment; filename=report.pdf"),
            Some("report.pdf".to_owned())
        );
    }

    #[test]
    fn test_extract_filename_none() {
        assert_eq!(extract_filename("inline"), None);
    }

    #[test]
    fn test_extract_filename_with_ext() {
        assert_eq!(
            extract_filename("attachment; filename*=UTF-8''%c3%a9.txt"),
            Some("\u{e9}.txt".to_owned())
        );
    }

    // -- Multiple parameters --

    #[test]
    fn test_multiple_parameters() {
        let result = parse_content_disposition(
            "attachment; size=1234; filename=\"test.txt\"; creation-date=\"Wed, 12 Feb 1997 16:29:51 -0500\"",
        );
        assert_eq!(result.disposition_type, "attachment");
        assert_eq!(result.filename.as_deref(), Some("test.txt"));
    }

    // -- Case insensitivity --

    #[test]
    fn test_case_insensitive_filename_param() {
        let result = parse_content_disposition("attachment; FILENAME=test.txt");
        assert_eq!(result.filename.as_deref(), Some("test.txt"));
    }

    #[test]
    fn test_case_insensitive_filename_star_param() {
        let result = parse_content_disposition("attachment; FILENAME*=utf-8''test.txt");
        assert_eq!(result.filename.as_deref(), Some("test.txt"));
    }

    // -- ISO-8859-1 conversion --

    #[test]
    fn test_iso8859p1_to_utf8_ascii() {
        assert_eq!(iso8859p1_to_utf8(b"hello"), Some("hello".to_owned()));
    }

    #[test]
    fn test_iso8859p1_to_utf8_extended() {
        // 0xE9 = é in ISO-8859-1 → UTF-8: 0xC3 0xA9
        let result = iso8859p1_to_utf8(&[0xE9]).unwrap();
        assert_eq!(result, "\u{e9}");
    }

    #[test]
    fn test_iso8859p1_to_utf8_c1_control_rejected() {
        // 0x80..0x9F are C1 control characters, rejected per C++ implementation
        assert!(iso8859p1_to_utf8(&[0x80]).is_none());
        assert!(iso8859p1_to_utf8(&[0x9F]).is_none());
    }

    #[test]
    fn test_iso8859p1_to_utf8_nbsp() {
        // 0xA0 = non-breaking space → UTF-8: 0xC2 0xA0
        let result = iso8859p1_to_utf8(&[0xA0]).unwrap();
        assert_eq!(result, "\u{a0}");
    }

    // -- hex_digit helper --

    #[test]
    fn test_hex_digit() {
        assert_eq!(hex_digit(b'0'), Some(0));
        assert_eq!(hex_digit(b'9'), Some(9));
        assert_eq!(hex_digit(b'a'), Some(10));
        assert_eq!(hex_digit(b'f'), Some(15));
        assert_eq!(hex_digit(b'A'), Some(10));
        assert_eq!(hex_digit(b'F'), Some(15));
        assert_eq!(hex_digit(b'g'), None);
        assert_eq!(hex_digit(b' '), None);
    }

    // -- is_dir_traversal --

    #[test]
    fn test_dir_traversal_patterns() {
        assert!(is_dir_traversal("."));
        assert!(is_dir_traversal(".."));
        assert!(is_dir_traversal("/"));
        assert!(is_dir_traversal("/etc/passwd"));
        assert!(is_dir_traversal("./secret"));
        assert!(is_dir_traversal("../secret"));
        assert!(is_dir_traversal("dir/./file"));
        assert!(is_dir_traversal("dir/../file"));
        assert!(is_dir_traversal("dir/"));
        assert!(is_dir_traversal("dir/."));
        assert!(is_dir_traversal("dir/.."));
        assert!(is_dir_traversal("dir\\file"));
        assert!(is_dir_traversal("\x01bad"));
    }

    #[test]
    fn test_valid_filenames_not_traversal() {
        assert!(!is_dir_traversal("file.txt"));
        assert!(!is_dir_traversal("hello world.pdf"));
        assert!(!is_dir_traversal("archive.tar.gz"));
        assert!(!is_dir_traversal(""));
    }

    // -- Real-world header values --

    #[test]
    fn test_real_world_attachment() {
        let result = parse_content_disposition(
            "attachment; filename=\"genome.jpeg\"; modification-date=\"Wed, 12 Feb 1997 16:29:51 -0500\";",
        );
        assert_eq!(result.disposition_type, "attachment");
        assert_eq!(result.filename.as_deref(), Some("genome.jpeg"));
    }

    #[test]
    fn test_real_world_utf8_filename_star() {
        let result = parse_content_disposition(
            "attachment; filename=\"hello.pdf\"; filename*=UTF-8''%e2%82%ac%20rates.pdf",
        );
        assert_eq!(result.filename.as_deref(), Some("\u{20ac} rates.pdf"));
        assert_eq!(result.filename_ascii.as_deref(), Some("hello.pdf"));
    }

    // -- Only filename* present (no filename=) --

    #[test]
    fn test_only_ext_filename() {
        let result = parse_content_disposition("attachment; filename*=UTF-8''test.txt");
        assert_eq!(result.filename.as_deref(), Some("test.txt"));
        assert!(result.filename_ascii.is_none());
    }

    // -- Only filename= present (no filename*) --

    #[test]
    fn test_only_plain_filename() {
        let result = parse_content_disposition("attachment; filename=test.txt");
        assert_eq!(result.filename.as_deref(), Some("test.txt"));
        assert_eq!(result.filename_ascii.as_deref(), Some("test.txt"));
    }

    // -- Non-filename parameters are ignored --

    #[test]
    fn test_non_filename_params_ignored() {
        let result =
            parse_content_disposition("form-data; name=\"fieldName\"; filename=\"file.dat\"");
        assert_eq!(result.disposition_type, "form-data");
        assert_eq!(result.filename.as_deref(), Some("file.dat"));
    }

    // -- Percent-encoding in filename* with multi-byte UTF-8 --

    #[test]
    fn test_percent_encoded_cjk() {
        // 日本語 in UTF-8: e6 97 a5 e6 9c ac e8 aa 9e
        let result = parse_content_disposition(
            "attachment; filename*=UTF-8''%e6%97%a5%e6%9c%ac%e8%aa%9e.txt",
        );
        assert_eq!(
            result.filename.as_deref(),
            Some("\u{65e5}\u{672c}\u{8a9e}.txt")
        );
    }

    // -- Filename with special token characters --

    #[test]
    fn test_filename_with_token_chars() {
        let result = parse_content_disposition("attachment; filename=file-v1.2_beta.txt");
        assert_eq!(result.filename.as_deref(), Some("file-v1.2_beta.txt"));
    }

    // -- Quoted string with backslash-escaped characters --

    #[test]
    fn test_quoted_backslash_escape() {
        // The parser correctly unescapes \\ to \ in quoted strings,
        // but filenames containing backslashes are rejected by the
        // directory-traversal check (is_dir_traversal rejects '\').
        let result = parse_content_disposition("attachment; filename=\"path\\\\to\\\\file.txt\"");
        assert_eq!(
            result.filename, None,
            "Backslash in filename should be rejected by dir traversal"
        );
    }

    #[test]
    fn test_quoted_escaped_backslash_then_char() {
        // \n in a quoted string is just the literal characters 'n' after backslash
        let result = parse_content_disposition("attachment; filename=\"hello\\nworld.txt\"");
        assert_eq!(result.filename.as_deref(), Some("hellonworld.txt"));
    }

    // -- Edge: filename= is ignored when filename* already found --

    #[test]
    fn test_filename_ignored_when_ext_already_found() {
        let result =
            parse_content_disposition("attachment; filename*=UTF-8''star.txt; filename=plain.txt");
        // RFC 6266: filename* takes priority for the `filename` field,
        // but filename= is still collected as filename_ascii fallback.
        assert_eq!(result.filename.as_deref(), Some("star.txt"));
        assert_eq!(result.filename_ascii.as_deref(), Some("plain.txt"));
    }

    // -- Quoted string with space --

    #[test]
    fn test_quoted_filename_with_spaces_in_value() {
        let result = parse_content_disposition("attachment; filename=\"my report.pdf\"");
        assert_eq!(result.filename.as_deref(), Some("my report.pdf"));
    }

    // -- Whitespace handling around semicolons --

    #[test]
    fn test_whitespace_around_semicolons() {
        let result = parse_content_disposition("attachment ; filename=test.txt");
        assert_eq!(result.filename.as_deref(), Some("test.txt"));
    }
}
