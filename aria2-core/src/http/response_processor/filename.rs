//! Resolve a safe local filename from an HTTP response and its effective URL.
//!
//! The module owns one small interface for the policy: an unambiguous
//! Content-Disposition value wins, then the final URL path segment, then the
//! aria2 compatibility fallback. HTTP parsing and local filename
//! normalization remain separate so the safety policy is applied uniformly.

use tracing::debug;
use url::Url;

use crate::http::content_disposition::parse_content_disposition_with_default_utf8;
use crate::http::header_processor::HttpResponseHead;
use crate::util::uri;

/// Default filename when the URI path ends with `/`.
/// Matches C++ `Request::DEFAULT_FILE`.
pub(crate) const DEFAULT_FILE: &str = "index.html";

/// Determine the output filename from response metadata or the effective URL.
///
/// A caller that already has an explicit output path must bypass this
/// function. The returned value is always a safe basename.
///
/// # Arguments
///
/// * `response_head` - Parsed HTTP response headers.
/// * `request_url` - The URL associated with this response. After a redirect,
///   callers should pass the final response URL.
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
    if let Some(filename) =
        content_disposition_filename(response_head, content_disposition_default_utf8)
    {
        debug!(
            filename = %filename,
            source = "Content-Disposition",
            "Filename determined"
        );
        return filename;
    }

    let filename = extract_filename_from_url(request_url);
    debug!(filename = %filename, source = "URL", "Filename determined");
    filename
}

/// Determine a filename directly from a reqwest response.
///
/// This is the adapter used by the download engine. Keeping the conversion
/// here prevents the reqwest path from growing a second Content-Disposition
/// parser or a second URL sanitization policy.
pub(crate) fn determine_filename_from_response(
    response: &reqwest::Response,
    content_disposition_default_utf8: bool,
) -> String {
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((
                name.as_str().to_ascii_lowercase(),
                value.to_str().ok()?.to_owned(),
            ))
        })
        .collect();
    let head = HttpResponseHead::new(
        format!("{:?}", response.version()),
        response.status().as_u16(),
        response
            .status()
            .canonical_reason()
            .unwrap_or_default()
            .to_owned(),
        headers,
    );
    determine_filename(
        &head,
        response.url().as_str(),
        content_disposition_default_utf8,
    )
}

/// Extract a filename from the URL path, preserving the raw segment until it
/// has been isolated from the directory path.
///
/// This ordering matters for encoded separators such as `%2F`: decoding the
/// entire path first would allow an encoded separator to change which segment
/// is treated as the basename.
/// Returns "index.html" if the path ends with `/` or is empty.
pub(crate) fn extract_filename_from_url(url: &str) -> String {
    let Some(raw_segment) = raw_url_path_segment(url) else {
        return DEFAULT_FILE.to_owned();
    };

    let decoded = uri::percent_decode(&raw_segment);
    sanitize_filename(&decoded).unwrap_or_else(|| DEFAULT_FILE.to_owned())
}

/// Return the only usable Content-Disposition value.
///
/// Multiple values are ambiguous and are ignored as a group. This makes the
/// result independent of header ordering and intermediary behaviour.
fn content_disposition_filename(
    response_head: &HttpResponseHead,
    default_utf8: bool,
) -> Option<String> {
    let values = response_head.header_all("content-disposition");
    let value = match values.as_slice() {
        [] => return None,
        [value] => value,
        _ => {
            debug!(
                count = values.len(),
                "Ignoring duplicate Content-Disposition headers"
            );
            return None;
        }
    };

    let candidate = parse_content_disposition_filename(value, default_utf8)?;
    sanitize_filename(&candidate)
}

/// Extract the raw final path segment without decoding it first.
fn raw_url_path_segment(url: &str) -> Option<String> {
    if let Ok(parsed) = Url::parse(url) {
        if parsed.path().ends_with('/') {
            return None;
        }
        return parsed
            .path()
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .map(str::to_owned);
    }

    // Keep a useful fallback for URI-like inputs accepted by the request
    // layer, while excluding query and fragment data.
    let without_suffix = url.split(['?', '#']).next().unwrap_or_default();
    let path = if let Some(scheme_end) = without_suffix.find("://") {
        let authority_and_path = &without_suffix[scheme_end + 3..];
        let slash = authority_and_path.find('/')?;
        &authority_and_path[slash..]
    } else {
        without_suffix
    };

    if path.ends_with('/') {
        return None;
    }

    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(str::to_owned)
}

/// Normalize one candidate into a safe, cross-platform basename.
///
/// Directory separators are untrusted path information and only the final
/// component is retained. Windows-invalid characters are replaced even on
/// Unix so a downloaded task remains portable.
fn sanitize_filename(candidate: &str) -> Option<String> {
    let basename = candidate
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())?;

    let mut name: String = basename
        .chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            _ => ch,
        })
        .collect();

    // Trailing spaces and dots are not stable across Windows and Unix.
    name = name.trim_end_matches([' ', '.']).to_owned();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    if is_windows_reserved_name(&name) {
        name.insert(0, '_');
    }

    if name.len() > MAX_FILENAME_BYTES {
        let mut end = MAX_FILENAME_BYTES;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        name = name.trim_end_matches([' ', '.']).to_owned();
    }

    (!name.is_empty()).then_some(name)
}

const MAX_FILENAME_BYTES: usize = 255;

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default();
    let bytes = stem.as_bytes();
    stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
        || (bytes.len() == 4
            && (bytes[..3].eq_ignore_ascii_case(b"COM") || bytes[..3].eq_ignore_ascii_case(b"LPT"))
            && bytes[3].is_ascii_digit()
            && bytes[3] != b'0')
}

/// Parse filename from Content-Disposition header value using the full
/// RFC 6266 state-machine parser.
///
/// This delegates to `content_disposition::parse_content_disposition_with_default_utf8()` which
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
fn parse_content_disposition_filename(cd_value: &str, default_utf8: bool) -> Option<String> {
    let result = parse_content_disposition_with_default_utf8(cd_value, default_utf8);

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
        let filename = determine_filename(&head, "http://example.com/path/my%20file.txt", false);
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

    #[test]
    fn test_filename_from_url_decodes_only_after_extracting_segment() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        let filename = determine_filename(
            &head,
            "https://example.com/archive/dir%2Freport.txt?token=ignored#fragment",
            false,
        );
        assert_eq!(filename, "report.txt");
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

    // ── filename normalization tests ────────────────────────────────────

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(
            sanitize_filename("normal.txt"),
            Some("normal.txt".to_string())
        );
        assert_eq!(
            sanitize_filename("path/to/file.txt"),
            Some("file.txt".to_string())
        );
        assert_eq!(sanitize_filename("win\\path"), Some("path".to_string()));
        assert_eq!(
            sanitize_filename("null\0byte"),
            Some("nullbyte".to_string())
        );
        assert_eq!(sanitize_filename("CON.txt"), Some("_CON.txt".to_string()));
        assert_eq!(sanitize_filename("CON "), Some("_CON".to_string()));
        assert_eq!(sanitize_filename("COM1."), Some("_COM1".to_string()));
        assert_eq!(sanitize_filename(".."), None);
    }

    #[test]
    fn test_content_disposition_default_utf8_is_honored() {
        let head = parse_head(
            "HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=\"café.txt\"\r\n\r\n"
                .as_bytes(),
        );

        assert_eq!(
            determine_filename(&head, "https://example.com/fallback.txt", true),
            "café.txt"
        );
        assert_eq!(
            determine_filename(&head, "https://example.com/fallback.txt", false),
            "cafÃ©.txt"
        );
    }

    #[test]
    fn test_duplicate_content_disposition_headers_fall_back_to_url() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Disposition: attachment; filename=first.txt\r\nContent-Disposition: attachment; filename=second.txt\r\n\r\n",
        );
        assert_eq!(
            determine_filename(&head, "https://example.com/fallback.txt", false),
            "fallback.txt"
        );
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
        let head =
            parse_head(b"HTTP/1.1 200 OK\r\nContent-Disposition: \r\nContent-Length: 100\r\n\r\n");
        let filename = determine_filename(&head, "http://example.com/data.csv", false);
        assert_eq!(filename, "data.csv");
    }
}
