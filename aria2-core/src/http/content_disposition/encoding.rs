//! Encoding and decoding helpers for Content-Disposition filename values.
//!
//! Handles UTF-8 validation, ISO-8859-1 to UTF-8 conversion, directory-traversal
//! detection, and the final decoding of raw filename bytes into `String`s.

use tracing::trace;

// ---------------------------------------------------------------------------
// Charset enum for filename* values
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Charset {
    Unknown,
    Utf8,
    Iso8859p1,
}

// ---------------------------------------------------------------------------
// UTF-8 validation helper
// ---------------------------------------------------------------------------

/// Validate a byte sequence as UTF-8, returning the decoded String or None.
/// This replaces the C++ DFA-based `utf8dfa` validation.
#[inline]
pub(super) fn validate_utf8(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(|s| s.to_owned())
}

// ---------------------------------------------------------------------------
// ISO-8859-1 to UTF-8 conversion
// ---------------------------------------------------------------------------

/// Convert ISO-8859-1 bytes to a UTF-8 String.
/// Returns `None` if the input contains bytes in the 0x80..0x9F range
/// (C1 control characters not valid in ISO-8859-1 per the C++ implementation).
pub(super) fn iso8859p1_to_utf8(src: &[u8]) -> Option<String> {
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
pub(super) fn is_dir_traversal(s: &str) -> bool {
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
// Decoding helpers
// ---------------------------------------------------------------------------

/// Decode the `filename=` parameter value (plain or quoted).
/// Validates bytes as UTF-8; if invalid, attempts ISO-8859-1 interpretation.
/// Rejects directory-traversal filenames.
pub(super) fn decode_filename_ascii(bytes: &[u8]) -> Option<String> {
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
pub(super) fn decode_filename_ext(bytes: &[u8], charset: Charset) -> Option<String> {
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
