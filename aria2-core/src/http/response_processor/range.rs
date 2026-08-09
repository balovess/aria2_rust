//! Range validation and Content-Range parsing.
//!
//! Handles validation of server Content-Range against requested ranges,
//! parsing of Content-Range header values, entity length computation,
//! and chunked transfer-encoding detection.

use crate::http::header_processor::HttpResponseHead;

/// Validate that the server's Content-Range satisfies the requested range.
///
/// Matches the range portion of C++ `HttpRequest::isRangeSatisfied()`:
/// - If `req_end == 0` (no explicit end requested), only checks that
///   `resp_start == req_start`.
/// - If `req_end > 0`, requires both the start and end bytes to match exactly.
///
/// The C++ method also compares the entity length. The response validator
/// performs that comparison because this helper does not receive it.
///
/// # Arguments
///
/// * `req_start` - Start byte of the requested range (inclusive).
/// * `req_end` - End byte of the requested range (inclusive). 0 = no end.
/// * `resp_start` - Start byte from Content-Range header (inclusive).
/// * `resp_end` - End byte from Content-Range header (inclusive).
///
/// # Returns
///
/// `Ok(())` if ranges satisfy the request, `Err` with description if not.
pub fn validate_response_range(
    req_start: u64,
    req_end: u64,
    resp_start: u64,
    resp_end: u64,
) -> std::result::Result<(), String> {
    // Start byte must always match exactly
    if req_start != resp_start {
        return Err(format!(
            "Range start mismatch: requested={}, server={}",
            req_start, resp_start
        ));
    }

    // If no explicit end was requested, start match is sufficient
    if req_end == 0 {
        return Ok(());
    }

    // C++ requires exact equality when an explicit end was requested.
    if resp_end != req_end {
        return Err(format!(
            "Range end mismatch: requested={}, server={}",
            req_end, resp_end
        ));
    }

    Ok(())
}

/// Parse Content-Range header: `bytes start-end/total`.
///
/// Returns `Some((start, end, total))` if the header is present and well-formed.
pub(crate) fn parse_content_range(response_head: &HttpResponseHead) -> Option<(u64, u64, u64)> {
    let value = response_head.header("content-range")?;
    parse_content_range_value(value)
}

/// Parse a Content-Range header value string.
///
/// Format: `bytes start-end/total` or `bytes */total`.
pub(crate) fn parse_content_range_value(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.trim();
    let range = value
        .strip_prefix("bytes")
        .map(|rest| rest.trim_start_matches([' ', '\t', '=']))
        .unwrap_or(value);
    let (range, total) = range.split_once('/')?;
    if range.trim() == "*" || total.trim() == "*" {
        return None;
    }
    let (start, end) = range.trim().split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = end.trim().parse().ok()?;
    let total = total.trim().parse().ok()?;
    Some((start, end, total))
}

/// Compute the entity length from response headers.
///
/// Entity length is the total size of the resource, which may differ from
/// Content-Length when Content-Range is present. For a 206 Partial Content
/// response, the entity length is the total size from Content-Range.
/// For a 200 response, it's the Content-Length.
pub(crate) fn compute_entity_length(response_head: &HttpResponseHead) -> u64 {
    // If Content-Range is present (206 response), use the total size
    if let Some(range) = parse_content_range(response_head) {
        return range.2;
    }

    // Otherwise use Content-Length
    response_head.content_length().unwrap_or(0)
}

/// Check if Transfer-Encoding is chunked.
pub(crate) fn is_chunked_transfer_encoding(response_head: &HttpResponseHead) -> bool {
    match response_head.header("transfer-encoding") {
        Some(te) => te.to_lowercase().contains("chunked"),
        None => false,
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
    fn test_parse_content_range() {
        assert_eq!(
            parse_content_range_value("bytes 0-499/1000"),
            Some((0, 499, 1000))
        );
        assert_eq!(
            parse_content_range_value("bytes 500-999/1000"),
            Some((500, 999, 1000))
        );
        assert_eq!(parse_content_range_value("bytes */1000"), None);
        assert_eq!(
            parse_content_range_value("bytes=0-499/1000"),
            Some((0, 499, 1000))
        );
        assert_eq!(
            parse_content_range_value("0-499/1000"),
            Some((0, 499, 1000))
        );
        assert_eq!(parse_content_range_value("bytes 0-0/1"), Some((0, 0, 1)));
    }

    #[test]
    fn test_parse_content_range_invalid() {
        assert_eq!(parse_content_range_value("not-bytes 0-499/1000"), None);
        assert_eq!(parse_content_range_value("bytes invalid"), None);
        assert_eq!(parse_content_range_value(""), None);
    }

    #[test]
    fn test_validate_response_range_match() {
        assert!(validate_response_range(0, 499, 0, 499).is_ok());
        assert!(validate_response_range(500, 999, 500, 999).is_ok());
        assert!(validate_response_range(0, 499, 0, 999).is_err());
        assert!(validate_response_range(0, 499, 0, 499).is_ok());
        // No explicit end requested (req_end=0): only start must match
        assert!(validate_response_range(0, 0, 0, 999).is_ok());
    }

    #[test]
    fn test_validate_response_range_start_mismatch() {
        let result = validate_response_range(0, 499, 100, 499);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("start mismatch"));
    }

    #[test]
    fn test_validate_response_range_end_mismatch() {
        let result = validate_response_range(0, 999, 0, 499);
        assert_eq!(
            result.unwrap_err(),
            "Range end mismatch: requested=999, server=499"
        );
    }

    #[test]
    fn test_validate_response_range_no_end_requested() {
        // req_end=0 means no explicit end, so any resp_end is fine
        assert!(validate_response_range(100, 0, 100, 200).is_ok());
        assert!(validate_response_range(100, 0, 100, 0).is_ok());
    }

    #[test]
    fn test_chunked_transfer_encoding() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        assert!(is_chunked_transfer_encoding(&head));

        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        assert!(!is_chunked_transfer_encoding(&head));
    }

    #[test]
    fn test_entity_length_from_content_range() {
        let head = parse_head(
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-99/5000\r\nContent-Length: 100\r\n\r\n",
        );
        assert_eq!(compute_entity_length(&head), 5000);
    }

    #[test]
    fn test_entity_length_from_content_length() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\n\r\n");
        assert_eq!(compute_entity_length(&head), 2048);
    }

    #[test]
    fn test_entity_length_unknown() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(compute_entity_length(&head), 0);
    }
}
