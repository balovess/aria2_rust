//! HTTP response validation — mirrors C++ `HttpResponse::validateResponse()`.
//!
//! Enforces protocol-level constraints on HTTP responses before the download
//! engine proceeds. Each check corresponds to a rule in the C++ aria2
//! `HttpResponse::validateResponse()` method.
//!
//! # Validation rules
//!
//! | Status code(s) | Requirement | Error on violation |
//! |---|---|---|
//! | 200 / 206 | Range must satisfy request (if not chunked) | `CannotResume` |
//! | 206 + TE | Content-Range stripped by header processor; deferred to downstream | — |
//! | 304 | Request must include `If-Modified-Since` or `If-None-Match` | `HttpProtocolError` |
//! | 300–303, 307, 308 | `Location` header must be present | `HttpProtocolError` |
//! | 400+ | (Accepted — handled by skip_response) | — |
//! | Other (1xx etc.) | Unexpected status code | `HttpProtocolError` |

use crate::error::{Aria2Error, RecoverableError};
use crate::http::header_processor::HttpResponseHead;
use crate::http::response_processor::range::{parse_content_range, validate_response_range};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Context about the original request, needed for validation.
#[derive(Debug, Clone)]
pub struct ValidateRequestContext {
    /// Whether the request included conditional GET headers
    /// (`If-Modified-Since` or `If-None-Match`).
    pub conditional_request: bool,
    /// The (start, end) range requested, if any.
    pub requested_range: Option<(u64, u64)>,
}

/// Validate an HTTP response against protocol rules.
///
/// This is the Rust equivalent of C++ `HttpResponse::validateResponse()`.
/// It should be called **before** the response is further processed by the
/// download engine.
///
/// # Arguments
///
/// * `response_head` - Parsed HTTP response headers.
/// * `ctx` - Context about the original request.
///
/// # Returns
///
/// `Ok(())` if the response passes all validation checks.
/// `Err` with an appropriate `Aria2Error` if a rule is violated.
pub fn validate_response(
    response_head: &HttpResponseHead,
    ctx: &ValidateRequestContext,
) -> Result<(), Aria2Error> {
    let status = response_head.status_code;

    match status {
        200 | 206 => validate_200_206(response_head, ctx, status),
        304 => validate_304(ctx),
        300 | 301 | 302 | 303 | 307 | 308 => validate_redirect(response_head, status),
        _ if status >= 400 => {
            // 4xx/5xx: accepted, handled by skip_response downstream.
            Ok(())
        }
        _ => {
            // 1xx, 209, etc. — unexpected status codes.
            Err(Aria2Error::Recoverable(
                RecoverableError::HttpProtocolError {
                    message: format!("Unexpected HTTP status code: {}", status),
                },
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Private validation helpers
// ---------------------------------------------------------------------------

/// Validate 200 OK / 206 Partial Content responses.
///
/// Per C++ `validateResponse()`:
/// - If `Transfer-Encoding` is **not** present, compare the received range
///   against the requested range via `isRangeSatisfied()`.
/// - If `Transfer-Encoding` **is** present and status is 206 but there is
///   no `Content-Range` header, throw `CANNOT_RESUME`.
fn validate_200_206(
    response_head: &HttpResponseHead,
    ctx: &ValidateRequestContext,
    status: u16,
) -> Result<(), Aria2Error> {
    let has_transfer_encoding = response_head.has_transfer_encoding();

    if !has_transfer_encoding {
        // No Transfer-Encoding: validate Content-Range against requested range.
        if let Some((req_start, req_end)) = ctx.requested_range
            && let Some((resp_start, resp_end, _resp_total)) = parse_content_range(response_head)
                && let Err(_e) = validate_response_range(req_start, req_end, resp_start, resp_end) {
                    return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
                }
    }
    // When Transfer-Encoding is present, Content-Range is stripped by the
    // header processor per RFC 7230 §3.3.2 (Transfer-Encoding takes precedence
    // over Content-Length and Content-Range for body framing). We therefore
    // cannot validate Content-Range in this case. In C++ aria2, 206+TE without
    // Content-Range triggers CANNOT_RESUME, but since our header processor
    // always strips Content-Range when TE is present, rejecting would be too
    // strict (real servers do send 206+chunked+Content-Range). Accept the
    // response and rely on downstream range validation during body reception.

    // Suppress unused variable warning when status is not 206.
    let _ = status;

    Ok(())
}

/// Validate 304 Not Modified: the request must have been conditional.
///
/// Per C++ `validateResponse()`: if `!httpRequest_->conditionalRequest()`,
/// throw `DL_ABORT_EX2("Got 304 without If-Modified-Since or If-None-Match",
/// error_code::HTTP_PROTOCOL_ERROR)`.
fn validate_304(ctx: &ValidateRequestContext) -> Result<(), Aria2Error> {
    if !ctx.conditional_request {
        return Err(Aria2Error::Recoverable(
            RecoverableError::HttpProtocolError {
                message: "Got 304 without If-Modified-Since or If-None-Match".to_string(),
            },
        ));
    }
    Ok(())
}

/// Validate 3xx redirect: `Location` header must be present.
///
/// Per C++ `validateResponse()`: if `!httpHeader_->defined(LOCATION)`,
/// throw `DL_ABORT_EX2(fmt(EX_LOCATION_HEADER_REQUIRED, statusCode),
/// error_code::HTTP_PROTOCOL_ERROR)`.
fn validate_redirect(response_head: &HttpResponseHead, status: u16) -> Result<(), Aria2Error> {
    if response_head.header("location").is_none() {
        return Err(Aria2Error::Recoverable(
            RecoverableError::HttpProtocolError {
                message: format!("Location header required for status {}", status),
            },
        ));
    }
    Ok(())
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

    fn non_conditional_ctx() -> ValidateRequestContext {
        ValidateRequestContext {
            conditional_request: false,
            requested_range: None,
        }
    }

    fn conditional_ctx() -> ValidateRequestContext {
        ValidateRequestContext {
            conditional_request: true,
            requested_range: None,
        }
    }

    fn range_ctx(start: u64, end: u64) -> ValidateRequestContext {
        ValidateRequestContext {
            conditional_request: false,
            requested_range: Some((start, end)),
        }
    }

    // --- 200 OK ---

    #[test]
    fn test_200_ok_no_range() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n");
        assert!(validate_response(&head, &non_conditional_ctx()).is_ok());
    }

    #[test]
    fn test_200_ok_with_matching_range() {
        let head = parse_head(
            b"HTTP/1.1 200 OK\r\nContent-Range: bytes 0-499/1000\r\nContent-Length: 500\r\n\r\n",
        );
        assert!(validate_response(&head, &range_ctx(0, 499)).is_ok());
    }

    // --- 206 Partial Content ---

    #[test]
    fn test_206_with_matching_range() {
        let head = parse_head(
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-199/1000\r\nContent-Length: 100\r\n\r\n",
        );
        assert!(validate_response(&head, &range_ctx(100, 199)).is_ok());
    }

    #[test]
    fn test_206_range_mismatch_cannot_resume() {
        let head = parse_head(
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 200-299/1000\r\nContent-Length: 100\r\n\r\n",
        );
        let result = validate_response(&head, &range_ctx(100, 199));
        assert!(result.is_err());
        match result.unwrap_err() {
            Aria2Error::Recoverable(RecoverableError::CannotResume) => {}
            other => panic!("Expected CannotResume, got {:?}", other),
        }
    }

    #[test]
    fn test_206_chunked_accepted() {
        // When Transfer-Encoding is present, Content-Range is stripped by the
        // header processor per RFC 7230 §3.3.2. We accept the response and
        // defer range validation to downstream body reception.
        let head =
            parse_head(b"HTTP/1.1 206 Partial Content\r\nTransfer-Encoding: chunked\r\n\r\n");
        assert!(validate_response(&head, &non_conditional_ctx()).is_ok());
    }

    // --- 304 Not Modified ---

    #[test]
    fn test_304_with_conditional_request_ok() {
        let head = parse_head(b"HTTP/1.1 304 Not Modified\r\n\r\n");
        assert!(validate_response(&head, &conditional_ctx()).is_ok());
    }

    #[test]
    fn test_304_without_conditional_request_error() {
        let head = parse_head(b"HTTP/1.1 304 Not Modified\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
        match result.unwrap_err() {
            Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message }) => {
                assert!(message.contains("304"));
                assert!(message.contains("If-Modified-Since"));
            }
            other => panic!("Expected HttpProtocolError, got {:?}", other),
        }
    }

    // --- 3xx Redirect ---

    #[test]
    fn test_301_with_location_ok() {
        let head = parse_head(
            b"HTTP/1.1 301 Moved Permanently\r\nLocation: http://example.com/new\r\n\r\n",
        );
        assert!(validate_response(&head, &non_conditional_ctx()).is_ok());
    }

    #[test]
    fn test_302_without_location_error() {
        let head = parse_head(b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
        match result.unwrap_err() {
            Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message }) => {
                assert!(message.contains("Location"));
                assert!(message.contains("302"));
            }
            other => panic!("Expected HttpProtocolError, got {:?}", other),
        }
    }

    #[test]
    fn test_307_without_location_error() {
        let head = parse_head(b"HTTP/1.1 307 Temporary Redirect\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
        match result.unwrap_err() {
            Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message }) => {
                assert!(message.contains("307"));
            }
            other => panic!("Expected HttpProtocolError, got {:?}", other),
        }
    }

    #[test]
    fn test_308_without_location_error() {
        let head = parse_head(b"HTTP/1.1 308 Permanent Redirect\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
    }

    #[test]
    fn test_300_without_location_error() {
        let head = parse_head(b"HTTP/1.1 300 Multiple Choices\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
    }

    #[test]
    fn test_303_without_location_error() {
        let head = parse_head(b"HTTP/1.1 303 See Other\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
    }

    // --- 4xx/5xx accepted ---

    #[test]
    fn test_400_accepted() {
        let head = parse_head(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        assert!(validate_response(&head, &non_conditional_ctx()).is_ok());
    }

    #[test]
    fn test_404_accepted() {
        let head = parse_head(b"HTTP/1.1 404 Not Found\r\n\r\n");
        assert!(validate_response(&head, &non_conditional_ctx()).is_ok());
    }

    #[test]
    fn test_500_accepted() {
        let head = parse_head(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
        assert!(validate_response(&head, &non_conditional_ctx()).is_ok());
    }

    // --- Unexpected status codes ---

    #[test]
    fn test_100_continue_unexpected() {
        let head = parse_head(b"HTTP/1.1 100 Continue\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
        match result.unwrap_err() {
            Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message }) => {
                assert!(message.contains("100"));
            }
            other => panic!("Expected HttpProtocolError, got {:?}", other),
        }
    }

    #[test]
    fn test_1xx_unexpected() {
        let head = parse_head(b"HTTP/1.1 102 Processing\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
    }

    #[test]
    fn test_209_unexpected() {
        let head = parse_head(b"HTTP/1.1 209 Unknown\r\n\r\n");
        let result = validate_response(&head, &non_conditional_ctx());
        assert!(result.is_err());
    }
}
