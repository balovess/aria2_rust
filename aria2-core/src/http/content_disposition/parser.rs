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

/// Whether a byte is in the ISO/IEC 8859-1 character set.
///
/// Mirrors C++ `util::isIso8859p1()`. Note that the C++ `CD_VALUE_CHARS` state
/// accepts *only* `inRFC5987AttrChar` bytes; this predicate is applied to
/// **percent-decoded** bytes in `CD_VALUE_CHARS_PCT_ENCODED2` when the extended
/// parameter declares `iso-8859-1` (see `attwithfn2231utf8-bad` in the C++
/// test-suite, which rejects `%82` because it is a C1 control code).
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

/// Flags tracking which filename variants have been *completely* parsed.
///
/// Mirrors the C++ `CD_FILENAME_FOUND` / `CD_EXT_FILENAME_FOUND` bits. A flag is
/// only raised once the parameter's value has been fully consumed (closing
/// quote, or a `;`/LWS terminating a token / ext-value) — this is what makes
/// duplicate detection work the same way it does in C++.
#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    filename_found: bool,
    ext_filename_found: bool,
}

/// Which filename buffer the parser is currently writing into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileTarget {
    None,
    Filename,
    ExtFilename,
}

/// Mark the current filename parameter's value as complete.
///
/// Mirrors the C++ `if (in_file_parm) { flags |= CD_*_FILENAME_FOUND; }` blocks.
/// `in_file_parm` is deliberately separate from `target`: C++ keeps
/// `in_file_parm == 0` for a `filename=` that appears *after* an already
/// accepted `filename*=`, so such a value is neither stored nor marked found.
#[inline]
fn commit_value(flags: &mut Flags, in_file_parm: bool, target: FileTarget) {
    if !in_file_parm {
        return;
    }
    match target {
        FileTarget::Filename => flags.filename_found = true,
        FileTarget::ExtFilename => flags.ext_filename_found = true,
        FileTarget::None => {}
    }
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

    // Value bytes of the extended parameter currently being parsed, regardless
    // of whether it is `filename*`. C++ runs its UTF-8 DFA over *every*
    // ext-value (the `utf8dfa()` calls sit outside the `if (in_file_parm)`
    // guards), so a bad UTF-8 sequence in e.g. `foo*=utf-8''...` must fail the
    // whole header. Validating the accumulated buffer at the value boundary is
    // equivalent to the incremental DFA: the DFA rejects exactly those byte
    // sequences that are not well-formed UTF-8, and a non-`UTF8_ACCEPT` state
    // at the boundary means a truncated sequence, which is also invalid UTF-8.
    let mut ext_value_bytes: Vec<u8> = Vec::new();

    // Which filename buffer are we currently writing into?
    // `FileTarget::None` means we are not inside a filename parameter.
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
                        // C++ only sets `in_file_parm` when no `filename*` has
                        // been accepted yet; otherwise the value is ignored
                        // entirely and CD_FILENAME_FOUND is never raised (so a
                        // second `filename=` after a `filename*=` is *not* a
                        // duplicate error).
                        in_file_parm = !flags.ext_filename_found;
                        // Rust-only extension: keep collecting the bytes so the
                        // `filename_ascii` fallback field stays populated. This
                        // is invisible to C++ parity because `filename*` still
                        // wins for the primary `filename` field.
                        file_target = FileTarget::Filename;
                        filename_bytes.clear();
                        state = ParseState::BeforeValue;
                    } else if parm_name.len() > 1 && parm_name.last() == Some(&b'*') {
                        // C++: `mark_first != mark_last - 1 && *(mark_last - 1) == '*'`
                        // — an ext-token must have at least one char before the
                        // trailing `*`, so a bare `*=` is an ordinary parameter.
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
                    // End of quoted string: the value is complete (possibly
                    // empty — C++ explicitly allows `filename=""`).
                    commit_value(&mut flags, in_file_parm, file_target);
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
                    commit_value(&mut flags, in_file_parm, file_target);
                    state = ParseState::BeforeParmName;
                } else if is_lws(c) {
                    commit_value(&mut flags, in_file_parm, file_target);
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
                    // (C++: `if (in_file_parm) { dp = dest; dlen = destlen; }`).
                    if in_file_parm && file_target == FileTarget::ExtFilename {
                        ext_filename_bytes.clear();
                    }
                    // Mirrors the C++ DFA reset performed in CD_CHARSET.
                    ext_value_bytes.clear();
                    state = ParseState::ValueChars;
                } else if c != b'-' && !c.is_ascii_alphanumeric() {
                    trace!("parse_raw: invalid language byte 0x{:02x}", c);
                    return None;
                }
            }
            ParseState::ValueChars => {
                if is_rfc5987_attr_char(c) {
                    ext_value_bytes.push(c);
                    match file_target {
                        FileTarget::Filename => filename_bytes.push(c),
                        FileTarget::ExtFilename => ext_filename_bytes.push(c),
                        FileTarget::None => {}
                    }
                } else if c == b'%' {
                    pctval = 0;
                    state = ParseState::ValueCharsPctEncoded1;
                } else if c == b';' || is_lws(c) {
                    // C++ requires the UTF-8 DFA to be back in UTF8_ACCEPT when
                    // the value ends (`attwithfn2231iso-bad` → -1).
                    if ext_charset == Charset::Utf8
                        && std::str::from_utf8(&ext_value_bytes).is_err()
                    {
                        trace!("parse_raw: invalid UTF-8 in utf-8 ext-value");
                        return None;
                    }
                    commit_value(&mut flags, in_file_parm, file_target);
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
                        // C++ validates the decoded octet against the declared
                        // charset here; `iso-8859-1` rejects C0/C1 controls
                        // (`attwithfn2231utf8-bad` → -1). UTF-8 is validated on
                        // the accumulated buffer at the value boundary.
                        if ext_charset == Charset::Iso8859p1 && !is_iso8859p1(byte) {
                            trace!(
                                "parse_raw: byte 0x{:02x} not valid in iso-8859-1 ext-value",
                                byte
                            );
                            return None;
                        }
                        ext_value_bytes.push(byte);
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

    // Handle end-of-input terminal states.
    //
    // aria2_rust intentionally **accepts** a trailing `;` — i.e. ending in
    // `ParseState::BeforeParmName`. RFC 6266 defines the parameter list as
    // `*( ";" disposition-parm )`, so zero or more trailing empty parameters
    // are legal. Upstream C++ aria2 rejects this: its terminal `switch(state)`
    // does not accept `CD_BEFORE_DISPOSITION_PARM_NAME`
    // (`attwithasciifilenamenqs`: `attachment; filename=foo.html ;` → -1). That
    // is a long-standing bug (GitHub issue #1118, open 5+ years) that breaks
    // downloads from S3 / CloudFront / nginx, which routinely emit a trailing
    // `;`. We deliberately diverge from C++ here. Empty parameters in the
    // *middle* of the header (`attachment; ;filename=foo`) are still rejected,
    // but by the `BeforeParmName` state handler — a non-token byte such as `;`
    // triggers `return None` — not by this terminal check.
    match state {
        ParseState::BeforeDispositionType
        | ParseState::AfterDispositionType
        | ParseState::DispositionType
        | ParseState::AfterValue
        | ParseState::BeforeParmName
        | ParseState::Token => {}
        ParseState::ValueChars => {
            if ext_charset == Charset::Utf8 && std::str::from_utf8(&ext_value_bytes).is_err() {
                trace!("parse_raw: invalid UTF-8 in utf-8 ext-value at end of input");
                return None;
            }
        }
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
