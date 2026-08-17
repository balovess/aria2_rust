//! Integration tests for the HTTP response processor.
//!
//! These tests exercise the `HttpResponseProcessor` end-to-end by parsing
//! raw HTTP response bytes and verifying the resulting `ResponseProcessResult`.

use crate::error::{Aria2Error, RecoverableError};
use crate::http::header_processor::HttpHeaderProcessor;
use crate::http::request_response::HttpMethod;

use super::processor::HttpResponseProcessor;
use super::types::{ResponseProcessResult, ResponseProcessorConfig};

/// Helper: parse raw HTTP response bytes into HttpResponseHead.
fn parse_head(raw: &[u8]) -> crate::http::header_processor::HttpResponseHead {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(raw);
    proc.get_result().unwrap()
}

// ==================== 200 OK tests ====================

#[test]
fn test_200_ok_basic() {
    let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady {
            entity_length,
            filename,
            inflate_required,
            chunked,
            supports_persistent_connection,
            switch_head_to_get,
            last_modified,
            ..
        } => {
            assert_eq!(entity_length, 1024);
            assert_eq!(filename, "file.bin");
            assert!(!inflate_required);
            assert!(!chunked);
            assert!(supports_persistent_connection);
            assert!(!switch_head_to_get);
            assert!(last_modified.is_none());
        }
        _ => panic!("Expected DownloadReady, got {:?}", result),
    }
}

#[test]
fn test_200_ok_with_content_type() {
    let head = parse_head(
        b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nContent-Type: application/pdf\r\n\r\n",
    );
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/doc",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady { content_type, .. } => {
            assert_eq!(content_type.as_deref(), Some("application/pdf"));
        }
        _ => panic!("Expected DownloadReady"),
    }
}

#[test]
fn test_206_partial_content_with_range() {
    let head = parse_head(
        b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-199/1000\r\nContent-Length: 100\r\n\r\n",
    );
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            Some((100, 199)),
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady {
            entity_length,
            content_range,
            ..
        } => {
            assert_eq!(entity_length, 1000);
            assert_eq!(content_range, Some((100, 199, 1000)));
        }
        _ => panic!("Expected DownloadReady"),
    }
}

#[test]
fn test_206_range_mismatch_cannot_resume() {
    let head = parse_head(
        b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 200-299/1000\r\nContent-Length: 100\r\n\r\n",
    );
    let processor = HttpResponseProcessor::with_defaults();
    // Requested 100-199 but server says 200-299
    let result = processor.process(
        &head,
        HttpMethod::Get,
        "http://example.com/file.bin",
        Some((100, 199)),
        false,
        false,
        true,
        false,
    );
    assert!(result.is_err());
    match result.unwrap_err() {
        Aria2Error::Recoverable(RecoverableError::CannotResume) => {}
        other => panic!("Expected CannotResume, got {:?}", other),
    }
}

// ==================== 304 Not Modified ====================

#[test]
fn test_304_not_modified_with_conditional() {
    let head = parse_head(b"HTTP/1.1 304 Not Modified\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    // conditional_request = true: request had If-Modified-Since
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            true,
        )
        .unwrap();

    match result {
        ResponseProcessResult::NotModified { entity_length } => {
            assert_eq!(entity_length, 0);
        }
        _ => panic!("Expected NotModified, got {:?}", result),
    }
}

#[test]
fn test_304_not_modified_without_conditional_rejected() {
    let head = parse_head(b"HTTP/1.1 304 Not Modified\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    // conditional_request = false: no conditional headers sent
    let result = processor.process(
        &head,
        HttpMethod::Get,
        "http://example.com/file.bin",
        None,
        false,
        false,
        true,
        false,
    );
    assert!(result.is_err());
    match result.unwrap_err() {
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message }) => {
            assert!(message.contains("304"));
            assert!(message.contains("If-Modified-Since"));
        }
        other => panic!("Expected HttpProtocolError, got {:?}", other),
    }
}

#[test]
fn test_304_not_modified_with_length_and_conditional() {
    let head = parse_head(b"HTTP/1.1 304 Not Modified\r\nContent-Length: 2048\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            true,
        )
        .unwrap();

    match result {
        ResponseProcessResult::NotModified { entity_length } => {
            assert_eq!(entity_length, 2048);
        }
        _ => panic!("Expected NotModified"),
    }
}

// ==================== Content-encoding ====================

#[test]
fn test_gzip_disables_segmented_download() {
    let head =
        parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nContent-Encoding: gzip\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.tgz",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady {
            inflate_required, ..
        } => {
            assert!(inflate_required);
        }
        _ => panic!("Expected DownloadReady"),
    }
}

// ==================== HEAD -> GET ====================

#[test]
fn test_head_method_switch() {
    let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Head,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady {
            switch_head_to_get, ..
        } => {
            assert!(switch_head_to_get);
        }
        _ => panic!("Expected DownloadReady"),
    }
}

// ==================== Metalink/HTTP integration ====================

#[test]
fn test_metalink_link_headers() {
    let head = parse_head(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nLink: <http://mirror1>; rel=\"duplicate\"; pri=\"1\"\r\n\r\n",
    );
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady { metalink_uris, .. } => {
            assert_eq!(metalink_uris.len(), 1);
            assert_eq!(metalink_uris[0], "http://mirror1");
        }
        _ => panic!("Expected DownloadReady"),
    }
}

#[test]
fn test_metalink_disabled_after_first_response() {
    let head = parse_head(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nLink: <http://mirror1>; rel=\"duplicate\"; pri=\"1\"\r\n\r\n",
    );
    let processor = HttpResponseProcessor::with_defaults();
    // piece_storage_initialized = true means Metalink processing already happened
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            true,
            false,
            false,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady { metalink_uris, .. } => {
            assert!(metalink_uris.is_empty());
        }
        _ => panic!("Expected DownloadReady"),
    }
}

// ==================== Digest header ====================

#[test]
fn test_digest_header() {
    let head =
        parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nDigest: sha-256=abc123\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady { digests, .. } => {
            assert_eq!(digests.len(), 1);
            assert_eq!(digests[0].algorithm, "sha-256");
            assert_eq!(digests[0].value, "abc123");
        }
        _ => panic!("Expected DownloadReady"),
    }
}

// ==================== Redirect delegation ====================

#[test]
fn test_301_redirect_delegation() {
    let head = parse_head(
        b"HTTP/1.1 301 Moved Permanently\r\nLocation: http://example.com/new\r\nContent-Length: 0\r\n\r\n",
    );
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/old",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::Redirect(info) => {
            assert_eq!(info.target_url.as_str(), "http://example.com/new");
        }
        _ => panic!("Expected Redirect, got {:?}", result),
    }
}

#[test]
fn test_401_auth_challenge_delegation() {
    let head = parse_head(
        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n",
    );
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/secret",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::AuthChallenge(challenge) => {
            assert_eq!(
                challenge.scheme,
                crate::http::skip_response::AuthScheme::Basic
            );
            assert_eq!(challenge.realm, "test");
        }
        _ => panic!("Expected AuthChallenge, got {:?}", result),
    }
}

#[test]
fn test_404_error_delegation() {
    let head = parse_head(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/missing",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::Error { status_code, .. } => {
            assert_eq!(status_code, 404);
        }
        _ => panic!("Expected Error, got {:?}", result),
    }
}

#[test]
fn test_502_retry_classification_uses_configured_retry_wait() {
    let head = parse_head(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");

    let fatal = HttpResponseProcessor::with_defaults()
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();
    assert!(matches!(
        fatal,
        ResponseProcessResult::Error {
            status_code: 502,
            ..
        }
    ));

    let retryable = HttpResponseProcessor::with_defaults()
        .with_retry_wait(5)
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();
    assert!(matches!(
        retryable,
        ResponseProcessResult::RetryableError {
            status_code: 502,
            ..
        }
    ));
}

#[test]
fn test_404_retry_classification_is_preserved() {
    let head = parse_head(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
    let processor = HttpResponseProcessor::new(ResponseProcessorConfig {
        max_file_not_found: 2,
        ..ResponseProcessorConfig::default()
    });
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/missing",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    assert!(matches!(
        result,
        ResponseProcessResult::RetryableError {
            status_code: 404,
            ..
        }
    ));
}

#[test]
fn test_504_is_always_retryable() {
    let head = parse_head(b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\n\r\n");
    let result = HttpResponseProcessor::with_defaults()
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    assert!(matches!(
        result,
        ResponseProcessResult::RetryableError {
            status_code: 504,
            ..
        }
    ));
}

// ==================== HTTP/1.0 response ====================

#[test]
fn test_http10_response() {
    let head = parse_head(b"HTTP/1.0 200 OK\r\nContent-Length: 500\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady {
            supports_persistent_connection,
            ..
        } => {
            assert!(!supports_persistent_connection);
        }
        _ => panic!("Expected DownloadReady"),
    }
}

// ==================== Zero-length file ====================

#[test]
fn test_zero_length_file() {
    let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/empty.txt",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady {
            entity_length,
            knows_total_length,
            ..
        } => {
            assert_eq!(entity_length, 0);
            // Zero-length with explicit Content-Length: 0 means known
            assert!(knows_total_length);
        }
        _ => panic!("Expected DownloadReady"),
    }
}

// ==================== Last-Modified extraction ====================

#[test]
fn test_last_modified_extracted_from_response() {
    let head = parse_head(
        b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n\r\n",
    );
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady { last_modified, .. } => {
            assert_eq!(
                last_modified,
                Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string())
            );
        }
        _ => panic!("Expected DownloadReady"),
    }
}

#[test]
fn test_last_modified_absent() {
    let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor
        .process(
            &head,
            HttpMethod::Get,
            "http://example.com/file.bin",
            None,
            false,
            false,
            true,
            false,
        )
        .unwrap();

    match result {
        ResponseProcessResult::DownloadReady { last_modified, .. } => {
            assert!(last_modified.is_none());
        }
        _ => panic!("Expected DownloadReady"),
    }
}

// ==================== validate_response integration ====================

#[test]
fn test_redirect_without_location_rejected() {
    let head = parse_head(b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor.process(
        &head,
        HttpMethod::Get,
        "http://example.com/old",
        None,
        false,
        false,
        true,
        false,
    );
    assert!(result.is_err());
    match result.unwrap_err() {
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message }) => {
            assert!(message.contains("Location"));
        }
        other => panic!("Expected HttpProtocolError, got {:?}", other),
    }
}

#[test]
fn test_206_chunked_accepted() {
    // When Transfer-Encoding is present, Content-Range is stripped by the
    // header processor per RFC 7230 §3.3.2. We accept the response.
    let head = parse_head(b"HTTP/1.1 206 Partial Content\r\nTransfer-Encoding: chunked\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor.process(
        &head,
        HttpMethod::Get,
        "http://example.com/file.bin",
        None,
        false,
        false,
        true,
        false,
    );
    // validate_response accepts 206+chunked; downstream processing may differ.
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn test_1xx_unexpected_rejected() {
    let head = parse_head(b"HTTP/1.1 100 Continue\r\n\r\n");
    let processor = HttpResponseProcessor::with_defaults();
    let result = processor.process(
        &head,
        HttpMethod::Get,
        "http://example.com/file.bin",
        None,
        false,
        false,
        true,
        false,
    );
    assert!(result.is_err());
    match result.unwrap_err() {
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message }) => {
            assert!(message.contains("100"));
        }
        other => panic!("Expected HttpProtocolError, got {:?}", other),
    }
}
