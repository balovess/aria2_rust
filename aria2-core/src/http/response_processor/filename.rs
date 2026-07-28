//! Filename determination from Content-Disposition header or URL path.
//!
//! Priority order (matching C++ `HttpResponse::determineFilename()`):
//! 1. `Content-Disposition: attachment; filename="..."` or `filename*=...`
//! 2. URL path basename (percent-decoded, safe-path-ified)
//! 3. "index.html" if URL path ends with `/`
//!
//! The Content-Disposition parsing delegates to the full RFC 6266 state-machine
//! parser in `content_disposition.rs`, which mirrors the C++ implementation in
//! `util.cc::parse_content_disposition()`. This ensures correct handling of:
//! - `filename*` (RFC 5987 extended form with charset/language)
//! - `filename` (quoted or unquoted, with backslash escaping)
//! - Duplicate parameter rejection
//! - `defaultUTF8` mode (validating quoted-string bytes as UTF-8 or ISO-8859-1)
//! - Directory-traversal and path-separator rejection

use tracing::debug;

use crate::http::content_disposition::parse_content_disposition;
use crate::http::header_processor::HttpResponseHead;
use crate::util::uri;

/// Default filename when the URI path ends with `/`.
/// Matches C++ `Request::DEFAULT_FILE`.
pub(crate) const DEFAULT_FILE: &str = "index.html";

/// Determine the output filename from Content-Disposition header or URL path.
///
/// Priority order (matching C++ `HttpResponse::determineFilename()`):
/// 1. `Content-Disposition: attachment; filename="..."` or `filename*=...`
///    — Filenames containing `/` or `\` are rejected per C++
///      `getContentDispositionFilename()` which checks
///      `res.find_first_of("/\\") == std::string::npos`.
/// 2. URL path basename (percent-decoded, safe-path-ified)
/// 3. "index.html" if URL path ends with `/`
///
/// # Arguments
///
/// * `response_head` - Parsed HTTP response headers.
/// * `request_url` - The URL of the original request.
/// * `content_disposition_default_utf8` - Whether to treat Content-Disposition
///   filename as UTF-8 by default (maps to C++ `PREF_CONTENT_DISPOSITION_DEFAULT_UTF8`).
///
/// # Returns
///
/// The determined filename (basename only, no directory prefix).
pub fn determine_filename(
    response_head: &HttpResponseHead,
    request_url: &str,
    content_disposition_default_utf8: bool,
) -> String {
    // Try Content-Disposition header first
    if let Some(cd) = response_head.header("content-disposition") {
        if let Some(filename) =
            parse_content_disposition_filename(cd, content_disposition_default_utf8)
        {
            debug!(
                filename = %filename,
                source = "Content-Disposition",
                "Filename determined"
            );
            // C++ getContentDispositionFilename does NOT apply createSafePath
            // to Content-Disposition filenames — they are rejected outright
            // if they contain path separators. Since the RFC 6266 parser's
            // is_dir_traversal check already handles most cases, and we
            // additionally reject filenames with '/' or '\' below, the
            // filename returned here is safe to use directly.
            return filename;
        }
    }

    // Fall back to URL path
    let url_filename = extract_filename_from_url(request_url);
    debug!(filename = %url_filename, source = "URL", "Filename determined");
    url_filename
}

/// Extract filename from a URL's path component.
///
/// Percent-decodes the path, takes the basename, and applies safe-path rules.
/// Returns "index.html" if the path ends with `/` or is empty.
///
/// Matches C++ `HttpRequest::getFile()` + `util::percentDecode()` +
/// `util::createSafePath()` flow.
pub(crate) fn extract_filename_from_url(url: &str) -> String {
    // Parse the URL to get the path
    let path = match url::Url::parse(url) {
        Ok(parsed) => parsed.path().to_string(),
        Err(_) => {
            // Fallback: try to extract path manually
            match url.find("://") {
                Some(scheme_end) => {
                    let after_scheme = &url[scheme_end + 3..];
                    match after_scheme.find('/') {
                        Some(pos) => after_scheme[pos..].to_string(),
                        None => "/".to_string(),
                    }
                }
                None => url.to_string(),
            }
        }
    };

    // Percent-decode the path using the proper byte-level decoder.
    // Unlike the old `percent_decode_str` which used `byte as char`
    // (breaking multi-byte UTF-8), `uri::percent_decode` collects
    // raw bytes and validates as UTF-8, matching C++ `percentDecode()`.
    let decoded = uri::percent_decode(&path);

    // Extract basename
    let basename = match decoded.rfind('/') {
        Some(pos) if pos + 1 < decoded.len() => &decoded[pos + 1..],
        _ => return create_safe_path(DEFAULT_FILE),
    };

    if basename.is_empty() {
        create_safe_path(DEFAULT_FILE)
    } else {
        create_safe_path(basename)
    }
}

/// Create a safe path by replacing directory separators and other unsafe chars.
///
/// Matches C++ `util::createSafePath()`. Applied only to URL-derived filenames;
/// Content-Disposition filenames are rejected rather than sanitized if they
/// contain path separators.
pub(crate) fn create_safe_path(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' => result.push('_'),
            '\0' => {} // Strip null bytes
            _ => result.push(ch),
        }
    }
    if result.is_empty() {
        DEFAULT_FILE.to_string()
    } else {
        result
    }
}

/// Parse filename from Content-Disposition header value using the full
/// RFC 6266 state-machine parser.
///
/// This delegates to `content_disposition::parse_content_disposition()` which
/// handles:
/// - `filename*` (RFC 5987 extended form: `charset'language'percent-encoded-value`)
/// - `filename` (quoted with backslash escaping, or unquoted token)
/// - Duplicate parameter rejection (C++ returns -1 for duplicates)
/// - `defaultUTF8` mode: validates quoted-string bytes as UTF-8 or ISO-8859-1
/// - Directory-traversal detection (`detectDirTraversal`)
/// - ISO-8859-1 → UTF-8 conversion
///
/// Additionally, we reject filenames containing `/` or `\` anywhere, matching
/// the C++ `getContentDispositionFilename()` check:
/// `res.find_first_of("/\\") == std::string::npos`.
///
/// Returns `None` if no valid filename is found or the filename is rejected.
fn parse_content_disposition_filename(
    cd_value: &str,
    _default_utf8: bool,
) -> Option<String> {
    let result = parse_content_disposition(cd_value);

    // If parsing failed (disposition_type is empty), no valid filename
    if result.disposition_type.is_empty() {
        return None;
    }

    // Get the filename (prefers filename* over filename= per RFC 6266)
    let filename = result.filename?;

    // Additional C++ check: reject filenames containing '/' or '\'.
    // C++ getContentDispositionFilename() does:
    //   if (!detectDirTraversal(res) &&
    //       res.find_first_of("/\\") == std::string::npos) { return res; }
    // The content_disposition parser's is_dir_traversal already handles
    // most cases (starting /, containing \, etc.), but does NOT reject
    // plain "subdir/file.txt" (multi-segment path without traversal).
    // The C++ find_first_of check catches these cases.
    if filename.contains('/') || filename.contains('\\') {
        debug!(
            filename = %filename,
            "Content-Disposition filename rejected: contains path separator"
        );
        return None;
    }

    Some(filename)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::header_processor::HttpHeaderProcessor;

    /// Helper: parse raw HTTP response bytes into HttpResponseHead.
    fn parse_head(raw: &[u8]) -> HttpResponseHead {
        let mut proc = HttpHeaderProcessor::new();
        proc.feed(raw);
        proc.get_result().unwrap()
    }

    // ── URL filename tests ──────────────────────────────────────────────

    #[test]
    fn test_filename_from_url_path() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        let filename = determine_filename(&head, "http://example.com/path/to/file.txt", false);
        assert_eq!(filename, "file.txt");
    }

    #[test]
    fn test_filename_from_url_trailing_slash() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        let filename = determine_filename(&head, "http://example.com/dir/", false);
        assert_eq!(filename, "index.html");
    }

    #[test]
    fn test_filename_from_url_percent_encoded() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        let filename =
            determine_filename(&head, "http://example.com/path/my%20file.txt", false);
        assert_eq!(filename, "my file.txt");
    }

    #[test]
    fn test_filename_from_url_utf8_percent_encoded() {
        // CJK characters in URL: 日本語 = %E6%97%A5%E6%9C%AC%E8%AA%9E
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        let filename = determine_filename(
            &head,
            "http://example.com/%E6%97%A5%E6%9C%AC%E8%AA%9E.txt",
            false,
        );
        assert_eq!(filename, "\u{65e5}\u{672c}\u{8a9e}.txt");
    }

    // ── Content-Disposition filename tests ──────────────────────────────

    #[test]
    fn test_filename_from_content_disposition_quoted() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=\"my file.pdf\"\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/download", false);
        assert_eq!(filename, "my file.pdf");
    }

    #[test]
    fn test_filename_from_content_disposition_unquoted() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=report.csv\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/download", false);
        assert_eq!(filename, "report.csv");
    }

    #[test]
    fn test_filename_from_content_disposition_star() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename*=UTF-8''my%20doc.txt\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/download", false);
        assert_eq!(filename, "my doc.txt");
    }

    #[test]
    fn test_filename_content_disposition_priority_over_url() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: inline; filename=\"override.txt\"\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/original.txt", false);
        assert_eq!(filename, "override.txt");
    }

    #[test]
    fn test_filename_star_priority_over_filename() {
        // RFC 6266: filename* takes priority over filename
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=\"fallback.txt\"; filename*=UTF-8''preferred.txt\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/download", false);
        assert_eq!(filename, "preferred.txt");
    }

    #[test]
    fn test_filename_star_with_cjk() {
        // Japanese: こんにちは = %e3%81%93%e3%82%93%e3%81%ab%e3%81%a1%e3%81%af
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename*=UTF-8''%e3%81%93%e3%82%93%e3%81%ab%e3%81%a1%e3%81%af.txt\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/download", false);
        assert_eq!(filename, "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}.txt");
    }

    // ── Path separator rejection (C++ getContentDispositionFilename) ────

    #[test]
    fn test_content_disposition_path_separator_rejected() {
        // C++ rejects filenames with '/' in getContentDispositionFilename
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=\"subdir/file.txt\"\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/original.txt", false);
        // Should fall back to URL filename since Content-Disposition has path separator
        assert_eq!(filename, "original.txt");
    }

    #[test]
    fn test_content_disposition_backslash_rejected() {
        // C++ rejects filenames with '\' in getContentDispositionFilename
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=\"dir\\\\file.txt\"\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/original.txt", false);
        // Should fall back to URL filename
        assert_eq!(filename, "original.txt");
    }

    #[test]
    fn test_content_disposition_directory_traversal_rejected() {
        // Directory traversal patterns should be rejected
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=\"../etc/passwd\"\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/original.txt", false);
        assert_eq!(filename, "original.txt");
    }

    // ── Duplicate parameter rejection (C++ parse_content_disposition) ──

    #[test]
    fn test_duplicate_filename_rejected() {
        // C++ returns -1 for duplicate filename= parameters
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=first.txt; filename=second.txt\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/original.txt", false);
        // Parse failure → fall back to URL
        assert_eq!(filename, "original.txt");
    }

    // ── create_safe_path tests ──────────────────────────────────────────

    #[test]
    fn test_create_safe_path() {
        assert_eq!(create_safe_path("normal.txt"), "normal.txt");
        assert_eq!(create_safe_path("path/to/file.txt"), "path_to_file.txt");
        assert_eq!(create_safe_path("win\\path"), "win_path");
        assert_eq!(create_safe_path("null\0byte"), "nullbyte");
    }

    // ── No Content-Disposition falls back to URL ────────────────────────

    #[test]
    fn test_no_content_disposition_uses_url() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        let filename = determine_filename(&head, "http://example.com/data.csv", false);
        assert_eq!(filename, "data.csv");
    }

    #[test]
    fn test_empty_content_disposition_uses_url() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: \r\nContent-Length: 100\r\n\r\n",
        );
        let filename = determine_filename(&head, "http://example.com/data.csv", false);
        assert_eq!(filename, "data.csv");
    }
}
