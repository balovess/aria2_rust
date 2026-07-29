//! RFC 6266 Content-Disposition state-machine parser.
//!
//! Contains the byte-level parsing logic that walks the header value through
//! the RFC-defined states, collecting raw filename bytes and charset
//! information. The actual decoding of those bytes into Rust `String`s is
//! handled by the [`encoding`](super::encoding) module.

use tracing::trace;

use super::encoding::Charset;

// ---------------------------------------------------------------------------
// Character classification helpers (matching C++ RFC helpers)
// ---------------------------------------------------------------------------

/// Characters allowed in an RFC 2616 token.
#[inline]
pub(super) fn is_rfc2616_http_token(c: u8) -> bool {
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
// Core parser
// ---------------------------------------------------------------------------

/// Parsed raw filename data from the state machine.
pub(super) struct RawFilename {
    /// Bytes collected for `filename=` (unquoted, raw bytes).
    pub filename_bytes: Vec<u8>,
    /// Bytes collected for `filename*=` (decoded, raw bytes).
    pub ext_filename_bytes: Vec<u8>,
    /// Charset detected in `filename*=...` parameter.
    pub ext_charset: Charset,
}

/// Parse a Content-Disposition header value and return the raw filename data
/// along with the disposition type.
///
/// This mirrors the C++ `parse_content_disposition` state machine but
/// returns structured data instead of writing into a fixed-size buffer.
pub(super) fn parse_raw(header_value: &str) -> Option<(String, RawFilename)> {
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
pub(super) fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
