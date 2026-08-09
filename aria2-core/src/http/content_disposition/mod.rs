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
//!
//! # Strictness (C++ parity)
//!
//! The C++ state machine is deliberately strict, and so is this port. The
//! following are **rejected outright** — the whole header fails and
//! [`ContentDispositionResult::disposition_type`] comes back empty:
//!
//! - An empty parameter, including a trailing `;`, is rejected. The C++
//!   terminal-state behavior is preserved: `attachment; ;filename=foo` and
//!   `attachment; filename=foo.html;` both fail parsing.
//! - An empty unquoted value: `attachment; filename=` and
//!   `attachment; filename=;` — a token may not be empty. An *empty quoted*
//!   value (`filename=""`) is legal and simply yields no filename, as is an
//!   empty ext-value (`filename*=UTF-8''`).
//! - A parameter with no `=`: `attachment; filename; x=y`.
//! - Two completed `filename=` parameters. Note that a `filename=` following an
//!   already accepted `filename*=` is *ignored* rather than counted, so it can
//!   never trigger the duplicate error (C++ leaves `in_file_parm == 0`).
//! - An ext-value whose octets do not match its declared charset: invalid UTF-8
//!   under `utf-8`, or non-ISO-8859-1 octets under `iso-8859-1`. This applies to
//!   *every* extended parameter, not just `filename*`.

mod encoding;
mod parser;

#[cfg(test)]
mod tests;

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
    let (disposition_type, raw) = match parser::parse_raw(header_value) {
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
    let filename_ascii = encoding::decode_filename_ascii(&raw.filename_bytes);

    // Decode filename*= (the extended parameter)
    let filename_ext = encoding::decode_filename_ext(&raw.ext_filename_bytes, raw.ext_charset);

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
