//! Connection persistence and content-encoding checks.
//!
//! Determines whether a connection supports keep-alive and whether
//! content-encoding (gzip/deflate) disables segmented download.

use tracing::debug;

use crate::http::header_processor::HttpResponseHead;

/// Check whether content-encoding would disable segmented download.
///
/// Per C++ `shouldInflateContentEncoding()`: on-the-fly inflation cannot
/// work with segment download because we don't know where each segment's
/// decompressed data should be written. So gzip/deflate content-encoding
/// forces single-connection download.
///
/// # Arguments
///
/// * `response_head` - Parsed HTTP response headers.
/// * `accept_gzip` - Whether the request included `Accept-Encoding: gzip`.
///
/// # Returns
///
/// `true` if content-encoding is gzip or deflate and the request accepted gzip.
pub fn should_inflate_content_encoding(
    response_head: &HttpResponseHead,
    accept_gzip: bool,
) -> bool {
    if !accept_gzip {
        return false;
    }
    match response_head.header("content-encoding") {
        Some(ce) => {
            let ce_lower = ce.to_lowercase();
            let result = ce_lower == "gzip" || ce_lower == "deflate";
            if result {
                debug!(encoding = %ce, "Content-encoding disables segmented download");
            }
            result
        }
        None => false,
    }
}

/// Check whether the server supports persistent (keep-alive) connections.
///
/// Per C++ `HttpResponse::supportsPersistentConnection()`:
/// - HTTP/1.1 defaults to keep-alive unless `Connection: close`.
/// - HTTP/1.0 defaults to close unless `Connection: keep-alive`.
/// - We don't trust non-HTTP/1.1 servers that send `Connection: keep-alive`.
pub fn supports_persistent_connection(response_head: &HttpResponseHead) -> bool {
    let connection_header = response_head.header("connection").map(|s| s.to_lowercase());

    match connection_header {
        Some(ref val) if val.contains("close") => false,
        Some(ref val) if val.contains("keep-alive") => {
            // Only trust keep-alive from HTTP/1.1 servers
            response_head.http_version == "HTTP/1.1"
        }
        None => {
            // Default: HTTP/1.1 = keep-alive, HTTP/1.0 = close
            response_head.http_version == "HTTP/1.1"
        }
        _ => false,
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
    fn test_http11_keep_alive_default() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        assert!(supports_persistent_connection(&head));
    }

    #[test]
    fn test_http11_connection_close() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
        assert!(!supports_persistent_connection(&head));
    }

    #[test]
    fn test_http10_no_keep_alive() {
        let head = parse_head(b"HTTP/1.0 200 OK\r\n\r\n");
        assert!(!supports_persistent_connection(&head));
    }

    #[test]
    fn test_http10_keep_alive_header() {
        // HTTP/1.0 with Connection: keep-alive — C++ says don't trust it
        let head = parse_head(b"HTTP/1.0 200 OK\r\nConnection: keep-alive\r\n\r\n");
        assert!(!supports_persistent_connection(&head));
    }

    #[test]
    fn test_http11_keep_alive_header() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\n\r\n");
        assert!(supports_persistent_connection(&head));
    }

    #[test]
    fn test_deflate_disables_segmented_download() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nContent-Encoding: deflate\r\n\r\n",
        );
        assert!(should_inflate_content_encoding(&head, true));
    }

    #[test]
    fn test_no_content_encoding() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n");
        assert!(!should_inflate_content_encoding(&head, true));
    }

    #[test]
    fn test_gzip_not_accepted() {
        let head =
            parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nContent-Encoding: gzip\r\n\r\n");
        // Even though server sent gzip, client didn't accept it
        assert!(!should_inflate_content_encoding(&head, false));
    }
}
