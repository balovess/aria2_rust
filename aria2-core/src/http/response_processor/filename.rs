//! Filename determination from Content-Disposition header or URL path.
//!
//! Priority order (matching C++ `HttpResponse::determineFilename()`):
//! 1. `Content-Disposition: attachment; filename="..."` or `filename*=...`
//! 2. URL path basename (percent-decoded, safe-path-ified)
//! 3. "index.html" if URL path ends with `/`

use tracing::{debug, warn};

use crate::http::header_processor::HttpResponseHead;

/// Default filename when the URI path ends with `/`.
pub(crate) const DEFAULT_FILE: &str = "index.html";

/// Determine the output filename from Content-Disposition header or URL path.
///
/// Priority order (matching C++ `HttpResponse::determineFilename()`):
/// 1. `Content-Disposition: attachment; filename="..."` or `filename*=...`
/// 2. URL path basename (percent-decoded, safe-path-ified)
/// 3. "index.html" if URL path ends with `/`
///
/// # Arguments
///
/// * `response_head` - Parsed HTTP response headers.
/// * `request_url` - The URL of the original request.
/// * `content_disposition_default_utf8` - Whether to treat Content-Disposition
///   filename as UTF-8 by default.
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
    if let Some(cd) = response_head.header("content-disposition")
        && let Some(filename) =
            parse_content_disposition_filename(cd, content_disposition_default_utf8)
        {
            debug!(filename = %filename, source = "Content-Disposition", "Filename determined");
            return create_safe_path(&filename);
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

    // Percent-decode the path
    let decoded = percent_decode_str(&path);

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

/// Percent-decode a string (simplified; handles common %XX sequences).
pub(crate) fn percent_decode_str(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Create a safe path by replacing directory separators and other unsafe chars.
///
/// Matches C++ `util::createSafePath()`.
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

/// Parse filename from Content-Disposition header value.
///
/// Handles both `filename="..."` and `filename*=...` forms per RFC 6266 / RFC 2183.
/// The `filename*` form uses RFC 5987 encoding: `charset'language'value`.
pub(crate) fn parse_content_disposition_filename(
    cd_value: &str,
    _default_utf8: bool,
) -> Option<String> {
    // Try filename* first (RFC 5987 extended form)
    if let Some(filename) = parse_filename_star(cd_value) {
        return Some(filename);
    }

    // Then try filename (quoted or unquoted)
    parse_filename_regular(cd_value)
}

/// Parse `filename*` from Content-Disposition header (RFC 5987).
fn parse_filename_star(cd_value: &str) -> Option<String> {
    // Find filename*= in the value
    let lower = cd_value.to_lowercase();
    let pos = lower.find("filename*=")?;
    let after = &cd_value[pos + "filename*=".len()..];
    let after = after.trim();

    // RFC 5987 format: charset'language'value
    // e.g. UTF-8''my%20file.txt
    let value = unquote_if_quoted(after);

    // Find the second single quote (charset'language'value)
    let first_quote = value.find('\'')?;
    let after_first = &value[first_quote + 1..];
    let second_quote = after_first.find('\'')?;
    let encoded_value = &after_first[second_quote + 1..];

    // The charset is before the first quote
    let charset = &value[..first_quote];

    // Percent-decode the value
    let decoded = percent_decode_str(encoded_value);

    // If charset is not UTF-8, log a warning but still return the decoded value
    if !charset.eq_ignore_ascii_case("utf-8") && !charset.eq_ignore_ascii_case("us-ascii") {
        warn!(charset = %charset, "Non-UTF-8 charset in Content-Disposition filename*");
    }

    Some(decoded)
}

/// Parse `filename` from Content-Disposition header (quoted or unquoted).
fn parse_filename_regular(cd_value: &str) -> Option<String> {
    // Find filename= in the value (but not filename*=)
    let lower = cd_value.to_lowercase();
    for prefix in &["filename=", "filename ="] {
        if let Some(pos) = lower.find(prefix) {
            // Make sure this isn't filename*=
            let before = &cd_value[..pos + prefix.len() - 1];
            if before.ends_with('*') || before.ends_with("* ") {
                continue;
            }
            let after = &cd_value[pos + prefix.len()..];
            let after = after.trim();
            let filename = unquote_if_quoted(after);
            // Trim trailing semicolons or whitespace
            let filename = filename.split(';').next().unwrap_or("").trim();
            if !filename.is_empty() {
                return Some(filename.to_string());
            }
        }
    }
    None
}

/// Remove surrounding double quotes from a string.
fn unquote_if_quoted(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
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
    fn test_create_safe_path() {
        assert_eq!(create_safe_path("normal.txt"), "normal.txt");
        assert_eq!(create_safe_path("path/to/file.txt"), "path_to_file.txt");
        assert_eq!(create_safe_path("win\\path"), "win_path");
    }

    #[test]
    fn test_percent_decode() {
        assert_eq!(percent_decode_str("hello%20world"), "hello world");
        assert_eq!(percent_decode_str("file%2Etxt"), "file.txt");
        assert_eq!(percent_decode_str("no-encoding"), "no-encoding");
        assert_eq!(percent_decode_str("%"), "%");
    }
}
