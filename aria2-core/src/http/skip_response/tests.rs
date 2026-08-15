//! Tests for the HTTP skip response handler.

use url::Url;

use crate::error::Aria2Error;
use crate::http::request_response::HttpMethod;

use super::handler::{HttpResponse, HttpSkipResponseHandler};
use super::types::*;

/// Helper to create an HttpResponse with a given status and optional Location header
fn make_response(status_code: u16, location: Option<&str>) -> HttpResponse {
    let mut resp = HttpResponse::new(status_code, "OK".to_string());
    if let Some(loc) = location {
        resp.headers.push(("Location".to_string(), loc.to_string()));
    }
    resp
}

/// Helper to create an HttpResponse with a WWW-Authenticate header
fn make_auth_response(status_code: u16, www_authenticate: &str) -> HttpResponse {
    let mut resp = HttpResponse::new(status_code, "OK".to_string());
    resp.headers
        .push(("WWW-Authenticate".to_string(), www_authenticate.to_string()));
    resp
}

/// Helper to create an HttpResponse with a Proxy-Authenticate header
fn make_proxy_auth_response(status_code: u16, proxy_authenticate: &str) -> HttpResponse {
    let mut resp = HttpResponse::new(status_code, "OK".to_string());
    resp.headers.push((
        "Proxy-Authenticate".to_string(),
        proxy_authenticate.to_string(),
    ));
    resp
}

// ==================== Redirect tests ====================

#[test]
fn test_redirect_301_permanent() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(301, Some("https://example.com/new"));
    let url = Url::parse("http://example.com/old").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::Redirect(info) => {
            assert_eq!(info.target_url.as_str(), "https://example.com/new");
            assert!(!info.change_method); // GET stays GET
            assert_eq!(info.redirect_type, RedirectType::Permanent);
            assert_eq!(info.redirect_count, 1);
        }
        _ => panic!("Expected Redirect result, got {:?}", result),
    }
}

#[test]
fn test_redirect_301_post_changes_to_get() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(301, Some("/new"));
    let url = Url::parse("http://example.com/old").unwrap();
    let result = handler.handle(&resp, HttpMethod::Post, &url, 0).unwrap();

    match result {
        SkipResponseResult::Redirect(info) => {
            assert!(info.change_method); // 301 changes POST to GET
            assert_eq!(info.redirect_type, RedirectType::Permanent);
        }
        _ => panic!("Expected Redirect result"),
    }
}

#[test]
fn test_redirect_302_temporary_changes_post() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(302, Some("/other"));
    let url = Url::parse("http://example.com/start").unwrap();

    // 302 with POST -> change method
    let result = handler.handle(&resp, HttpMethod::Post, &url, 0).unwrap();
    match result {
        SkipResponseResult::Redirect(info) => {
            assert!(info.change_method); // 302 historically changes POST->GET
            assert_eq!(info.redirect_type, RedirectType::Temporary);
        }
        _ => panic!("Expected Redirect result"),
    }

    // 302 with GET -> no change
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();
    match result {
        SkipResponseResult::Redirect(info) => {
            assert!(!info.change_method);
        }
        _ => panic!("Expected Redirect result"),
    }
}

#[test]
fn test_redirect_303_always_changes_method() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(303, Some("/result"));
    let url = Url::parse("http://example.com/submit").unwrap();

    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();
    match result {
        SkipResponseResult::Redirect(info) => {
            assert!(info.change_method); // 303 always changes method
            assert_eq!(info.redirect_type, RedirectType::SeeOther);
        }
        _ => panic!("Expected Redirect result"),
    }
}

#[test]
fn test_redirect_307_preserves_method() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(307, Some("/temp"));
    let url = Url::parse("http://example.com/submit").unwrap();
    let result = handler.handle(&resp, HttpMethod::Post, &url, 0).unwrap();

    match result {
        SkipResponseResult::Redirect(info) => {
            assert!(!info.change_method); // 307 preserves POST
            assert_eq!(info.redirect_type, RedirectType::PreserveMethod);
        }
        _ => panic!("Expected Redirect result"),
    }
}

#[test]
fn test_redirect_308_preserves_method() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(308, Some("/perm"));
    let url = Url::parse("http://example.com/submit").unwrap();
    let result = handler.handle(&resp, HttpMethod::Post, &url, 0).unwrap();

    match result {
        SkipResponseResult::Redirect(info) => {
            assert!(!info.change_method); // 308 preserves POST
            assert_eq!(info.redirect_type, RedirectType::PreserveMethod);
        }
        _ => panic!("Expected Redirect result"),
    }
}

#[test]
fn test_redirect_too_many() {
    let handler = HttpSkipResponseHandler::new(2);
    let resp = make_response(302, Some("/other"));
    let url = Url::parse("http://example.com/start").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 2);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("Too many redirects"));
}

#[test]
fn test_redirect_missing_location() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(302, None); // No Location header
    let url = Url::parse("http://example.com/start").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("Location"));
}

#[test]
fn test_redirect_relative_url_resolution() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(302, Some("/new-path?q=1"));
    let url = Url::parse("http://example.com/old-path").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::Redirect(info) => {
            assert_eq!(info.target_url.as_str(), "http://example.com/new-path?q=1");
        }
        _ => panic!("Expected Redirect result"),
    }
}

// ==================== Auth challenge tests ====================

#[test]
fn test_error_401_with_basic_challenge() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_http_auth_challenge(true);
    let resp = make_auth_response(401, r#"Basic realm="Secure Area""#);
    let url = Url::parse("http://example.com/protected").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::AuthChallenge(challenge) => {
            assert_eq!(challenge.scheme, AuthScheme::Basic);
            assert_eq!(challenge.realm, "Secure Area");
            assert!(!challenge.is_proxy);
        }
        _ => panic!("Expected AuthChallenge result, got {:?}", result),
    }
}

#[test]
fn test_error_401_with_digest_challenge() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_auth_response(
        401,
        r#"Digest realm="Downloads", nonce="abc123", qop="auth", algorithm="MD5""#,
    );
    let url = Url::parse("http://example.com/protected").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::AuthChallenge(challenge) => {
            assert_eq!(challenge.scheme, AuthScheme::Digest);
            assert_eq!(challenge.realm, "Downloads");
            assert!(!challenge.is_proxy);
            let digest = challenge.digest_challenge.unwrap();
            assert_eq!(digest.nonce, "abc123");
            assert_eq!(digest.qop.as_deref(), Some("auth"));
        }
        _ => panic!("Expected AuthChallenge result"),
    }
}

#[test]
fn test_error_407_proxy_auth() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_proxy_auth_response(407, r#"Basic realm="Proxy""#);
    let url = Url::parse("http://example.com/file").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::AuthChallenge(challenge) => {
            assert!(challenge.is_proxy);
            assert_eq!(challenge.scheme, AuthScheme::Basic);
            assert_eq!(challenge.realm, "Proxy");
        }
        _ => panic!("Expected AuthChallenge result"),
    }
}

#[test]
fn test_error_401_no_auth_header_with_challenge_enabled() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_http_auth_challenge(true);
    let resp = HttpResponse::new(401, "Unauthorized".to_string());
    let url = Url::parse("http://example.com/protected").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::AuthChallenge(challenge) => {
            assert_eq!(challenge.scheme, AuthScheme::Basic);
            assert!(!challenge.is_proxy);
        }
        _ => panic!("Expected AuthChallenge when http_auth_challenge_enabled=true"),
    }
}

#[test]
fn test_error_401_no_auth_header_without_challenge_enabled() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_http_auth_challenge(false);
    let resp = HttpResponse::new(401, "Unauthorized".to_string());
    let url = Url::parse("http://example.com/protected").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::FatalError { status_code, .. } => {
            assert_eq!(status_code, 401);
        }
        _ => panic!("Expected FatalError when http_auth_challenge_enabled=false"),
    }
}

// ==================== Error status tests ====================

#[test]
fn test_error_404_fatal_when_max_is_zero() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_max_file_not_found(0);
    let resp = make_response(404, None);
    let url = Url::parse("http://example.com/missing").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::FatalError { status_code, .. } => {
            assert_eq!(status_code, 404);
        }
        _ => panic!("Expected FatalError for 404 when max_file_not_found=0"),
    }
}

#[test]
fn test_error_404_retryable_when_max_is_nonzero() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_max_file_not_found(3);
    let resp = make_response(404, None);
    let url = Url::parse("http://example.com/missing").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::RetryableError { status_code, .. } => {
            assert_eq!(status_code, 404);
        }
        _ => panic!("Expected RetryableError for 404 when max_file_not_found>0"),
    }
}

#[test]
fn test_error_502_retryable_when_retry_wait_set() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_retry_wait(5);
    let resp = make_response(502, None);
    let url = Url::parse("http://example.com/file").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::RetryableError { status_code, .. } => {
            assert_eq!(status_code, 502);
        }
        _ => panic!("Expected RetryableError for 502 when retry_wait>0"),
    }
}

#[test]
fn test_error_502_fatal_when_retry_wait_zero() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_retry_wait(0);
    let resp = make_response(502, None);
    let url = Url::parse("http://example.com/file").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::FatalError { status_code, .. } => {
            assert_eq!(status_code, 502);
        }
        _ => panic!("Expected FatalError for 502 when retry_wait=0"),
    }
}

#[test]
fn test_error_503_same_as_502() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_retry_wait(2);
    let resp = make_response(503, None);
    let url = Url::parse("http://example.com/file").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::RetryableError { status_code, .. } => {
            assert_eq!(status_code, 503);
        }
        _ => panic!("Expected RetryableError for 503 when retry_wait>0"),
    }
}

#[test]
fn test_error_504_always_retryable() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_retry_wait(0);
    let resp = make_response(504, None);
    let url = Url::parse("http://example.com/file").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::RetryableError { status_code, .. } => {
            assert_eq!(status_code, 504);
        }
        _ => panic!("Expected RetryableError for 504"),
    }
}

#[test]
fn test_error_500_fatal() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(500, None);
    let url = Url::parse("http://example.com/file").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::FatalError {
            status_code,
            message,
        } => {
            assert_eq!(status_code, 500);
            assert!(message.contains("500"));
        }
        _ => panic!("Expected FatalError for generic 5xx"),
    }
}

#[test]
fn test_error_403_forbidden() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(403, None);
    let url = Url::parse("http://example.com/forbidden").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::FatalError { status_code, .. } => {
            assert_eq!(status_code, 403);
        }
        _ => panic!("Expected FatalError for 403"),
    }
}

#[test]
fn test_success_status_consumed() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(200, None);
    let url = Url::parse("http://example.com/file").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    assert!(matches!(result, SkipResponseResult::Consumed));
}

// ==================== Body consumption tests ====================

#[test]
fn test_consume_body_with_data() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let mut resp = HttpResponse::new(500, "Internal Server Error".to_string());
    resp.body = b"Error body content here".to_vec();
    let url = Url::parse("http://example.com/file").unwrap();

    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::FatalError { status_code, .. } => {
            assert_eq!(status_code, 500);
        }
        _ => panic!("Expected FatalError"),
    }
}

#[test]
fn test_consume_body_empty() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = HttpResponse::new(404, "Not Found".to_string());
    let url = Url::parse("http://example.com/missing").unwrap();

    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();
    match result {
        SkipResponseResult::FatalError { status_code, .. } => {
            assert_eq!(status_code, 404);
        }
        _ => panic!("Expected FatalError"),
    }
}

// ==================== Utility tests ====================

#[test]
fn test_extract_realm_quoted() {
    let realm = HttpSkipResponseHandler::extract_realm(r#"Basic realm="My Realm""#);
    assert_eq!(realm, "My Realm");
}

#[test]
fn test_extract_realm_unquoted() {
    let realm = HttpSkipResponseHandler::extract_realm("Basic realm=testrealm");
    assert_eq!(realm, "testrealm");
}

#[test]
fn test_extract_realm_missing() {
    let realm = HttpSkipResponseHandler::extract_realm("Negotiate");
    assert!(realm.is_empty());
}

#[test]
fn test_extract_realm_with_trailing_params() {
    let realm = HttpSkipResponseHandler::extract_realm(r#"Digest realm="test", nonce="abc""#);
    assert_eq!(realm, "test");
}

#[test]
fn test_auth_scheme_from_header() {
    assert_eq!(
        AuthScheme::from_header(r#"Basic realm="x""#),
        Some(AuthScheme::Basic)
    );
    assert_eq!(
        AuthScheme::from_header(r#"Digest realm="x", nonce="y""#),
        Some(AuthScheme::Digest)
    );
    assert_eq!(
        AuthScheme::from_header("Negotiate"),
        Some(AuthScheme::Negotiate)
    );
    assert_eq!(AuthScheme::from_header("NTLM"), Some(AuthScheme::Ntlm));
    assert_eq!(AuthScheme::from_header("UnknownScheme"), None);
    assert_eq!(AuthScheme::from_header(""), None);
}

#[test]
fn test_redirect_type_should_change_method() {
    // SeeOther always changes
    assert!(RedirectType::SeeOther.should_change_method(HttpMethod::Get));
    assert!(RedirectType::SeeOther.should_change_method(HttpMethod::Post));

    // Permanent changes POST -> GET
    assert!(RedirectType::Permanent.should_change_method(HttpMethod::Post));
    assert!(!RedirectType::Permanent.should_change_method(HttpMethod::Get));

    // Temporary (302) changes POST -> GET
    assert!(RedirectType::Temporary.should_change_method(HttpMethod::Post));
    assert!(!RedirectType::Temporary.should_change_method(HttpMethod::Get));

    // PreserveMethod (307/308) never changes
    assert!(!RedirectType::PreserveMethod.should_change_method(HttpMethod::Post));
    assert!(!RedirectType::PreserveMethod.should_change_method(HttpMethod::Get));
}

// ==================== 300 Multiple Choices tests ====================

#[test]
fn test_300_with_location_redirects() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = make_response(300, Some("http://example.com/choice1"));
    let url = Url::parse("http://example.com/list").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::Redirect(info) => {
            assert_eq!(info.target_url.as_str(), "http://example.com/choice1");
            assert_eq!(info.redirect_type, RedirectType::Permanent);
        }
        _ => panic!("Expected Redirect for 300 with Location, got {:?}", result),
    }
}

#[test]
fn test_300_without_location_is_fatal() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = HttpResponse::new(300, "Multiple Choices".to_string());
    let url = Url::parse("http://example.com/list").unwrap();
    let result = handler.handle(&resp, HttpMethod::Get, &url, 0).unwrap();

    match result {
        SkipResponseResult::FatalError { status_code, .. } => {
            assert_eq!(status_code, 300);
        }
        _ => panic!(
            "Expected FatalError for 300 without Location, got {:?}",
            result
        ),
    }
}

#[test]
fn test_unsupported_transfer_encoding_is_http_protocol_error() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let mut response = HttpResponse::new(404, "Not Found".to_string());
    response
        .headers
        .push(("Transfer-Encoding".to_string(), "gzip".to_string()));
    let url = Url::parse("http://example.com/missing").unwrap();

    let error = handler
        .handle(&response, HttpMethod::Get, &url, 0)
        .expect_err("unsupported transfer encoding must fail before status handling");

    assert!(matches!(error, Aria2Error::HttpProtocol(message) if message.contains("gzip")));
}

// ==================== 413 Request Entity Too Large tests ====================

#[test]
fn test_413_with_retry_after_is_retryable() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let mut resp = HttpResponse::new(413, "Payload Too Large".to_string());
    resp.headers
        .push(("Retry-After".to_string(), "60".to_string()));
    let url = Url::parse("http://example.com/upload").unwrap();
    let result = handler.handle(&resp, HttpMethod::Post, &url, 0).unwrap();

    match result {
        SkipResponseResult::RetryableError { status_code, .. } => {
            assert_eq!(status_code, 413);
        }
        _ => panic!(
            "Expected RetryableError for 413 with Retry-After, got {:?}",
            result
        ),
    }
}

#[test]
fn test_413_without_retry_after_is_fatal() {
    let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
    let resp = HttpResponse::new(413, "Payload Too Large".to_string());
    let url = Url::parse("http://example.com/upload").unwrap();
    let result = handler.handle(&resp, HttpMethod::Post, &url, 0).unwrap();

    match result {
        SkipResponseResult::FatalError { status_code, .. } => {
            assert_eq!(status_code, 413);
        }
        _ => panic!(
            "Expected FatalError for 413 without Retry-After, got {:?}",
            result
        ),
    }
}
