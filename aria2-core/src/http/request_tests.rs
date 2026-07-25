//! HTTP request builder integration tests
//!
//! Tests for HttpRequestBuilder fluent API, auto-generated headers,
//! and all conditional headers matching C++ aria2's createRequest().

use std::collections::HashMap;
use url::Url;

use crate::http::request::{HttpMethod, HttpRequestBuilder, basic_auth, bearer_token};

#[test]
fn test_request_builder_fluent_api() {
    let url = Url::parse("http://example.com/api/test").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Post, url.clone())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(b"{\"key\":\"value\"}".to_vec())
        .build()
        .unwrap();

    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(request.url, url);
    assert_eq!(request.headers.get("Content-Type").unwrap(), "application/json");
    assert_eq!(request.headers.get("Accept").unwrap(), "application/json");
    assert!(request.body.is_some());
    assert_eq!(request.body.unwrap(), b"{\"key\":\"value\"}");
}

#[test]
fn test_request_auto_headers_generation() {
    let url = Url::parse("http://example.com:8080/path").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .build()
        .unwrap();

    // Host with explicit port
    assert_eq!(request.headers.get("Host").unwrap(), "example.com:8080");
    assert_eq!(request.headers.get("User-Agent").unwrap(), "aria2-rust/1.0");
    assert_eq!(request.headers.get("Accept").unwrap(), "*/*");
    // Default: no Connection header (HTTP/1.1 keep-alive)
    assert!(request.headers.get("Connection").is_none());
}

#[test]
fn test_request_auto_headers_default_port_omitted() {
    // HTTP port 80 should be omitted from Host
    let url = Url::parse("http://example.com/path").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .build()
        .unwrap();
    assert_eq!(request.headers.get("Host").unwrap(), "example.com");

    // HTTPS port 443 should be omitted from Host
    let url_https = Url::parse("https://example.com/path").unwrap();
    let request_https = HttpRequestBuilder::new(HttpMethod::Get, url_https)
        .build()
        .unwrap();
    assert_eq!(request_https.headers.get("Host").unwrap(), "example.com");
}

#[test]
fn test_request_auto_content_length() {
    let url = Url::parse("http://example.com/api").unwrap();
    let body = b"test body data";
    let request = HttpRequestBuilder::new(HttpMethod::Post, url)
        .body(body.to_vec())
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Content-Length").unwrap(), &body.len().to_string());
}

#[test]
fn test_request_custom_host_not_overridden() {
    let url = Url::parse("http://example.com/api").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .header("Host", "custom-host.com")
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Host").unwrap(), "custom-host.com");
}

#[test]
fn test_request_to_bytes_with_body() {
    let url = Url::parse("http://example.com/api").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Post, url)
        .header("Content-Type", "text/plain")
        .body(b"Hello, World!".to_vec())
        .build()
        .unwrap();

    let bytes = request.to_bytes();
    let request_str = String::from_utf8_lossy(&bytes);
    assert!(request_str.contains("POST /api HTTP/1.1"));
    assert!(request_str.contains("Content-Length: 13"));
    assert!(request_str.ends_with("Hello, World!"));
}

#[test]
fn test_request_builder_batch_headers() {
    let url = Url::parse("http://example.com/api").unwrap();
    let mut custom_headers = HashMap::new();
    custom_headers.insert("X-Custom-1".to_string(), "value1".to_string());
    custom_headers.insert("X-Custom-2".to_string(), "value2".to_string());

    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .headers(custom_headers)
        .build()
        .unwrap();

    assert_eq!(request.headers.get("X-Custom-1").unwrap(), "value1");
    assert_eq!(request.headers.get("X-Custom-2").unwrap(), "value2");
}

// ==================== Accept-Encoding tests ====================

#[test]
fn test_accept_gzip_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .accept_gzip(true)
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Accept-Encoding").unwrap(), "deflate, gzip");
}

#[test]
fn test_accept_gzip_disabled_by_default() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .build()
        .unwrap();

    assert!(request.headers.get("Accept-Encoding").is_none());
}

// ==================== No-cache tests ====================

#[test]
fn test_no_cache_headers() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .no_cache(true)
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Pragma").unwrap(), "no-cache");
    assert_eq!(request.headers.get("Cache-Control").unwrap(), "no-cache");
}

#[test]
fn test_no_cache_disabled_by_default() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .build()
        .unwrap();

    assert!(request.headers.get("Pragma").is_none());
    assert!(request.headers.get("Cache-Control").is_none());
}

// ==================== Want-Digest tests ====================

#[test]
fn test_want_digest_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .want_digest(true)
        .build()
        .unwrap();

    // C++ format with q-values
    assert_eq!(
        request.headers.get("Want-Digest").unwrap(),
        "SHA-512;q=1, SHA-256;q=1, SHA;q=0.1"
    );
}

// ==================== Conditional GET tests ====================

#[test]
fn test_if_modified_since_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .if_modified_since("Wed, 21 Oct 2015 07:28:00 GMT".to_string())
        .build()
        .unwrap();

    assert_eq!(
        request.headers.get("If-Modified-Since").unwrap(),
        "Wed, 21 Oct 2015 07:28:00 GMT"
    );
}

#[test]
fn test_if_none_match_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .if_none_match("\"etag-123\"".to_string())
        .build()
        .unwrap();

    assert_eq!(request.headers.get("If-None-Match").unwrap(), "\"etag-123\"");
}

// ==================== Referer tests ====================

#[test]
fn test_referer_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .referer("http://referrer.example.com/page".to_string())
        .build()
        .unwrap();

    assert_eq!(
        request.headers.get("Referer").unwrap(),
        "http://referrer.example.com/page"
    );
}

#[test]
fn test_auto_referer() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .auto_referer()
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Referer").unwrap(), "http://example.com");
}

// ==================== Cookie tests ====================

#[test]
fn test_cookie_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .cookie("session=abc123; user=john".to_string())
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Cookie").unwrap(), "session=abc123; user=john");
}

// ==================== Authorization tests ====================

#[test]
fn test_authorization_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .authorization(basic_auth("user", "pass"))
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Authorization").unwrap(), "Basic dXNlcjpwYXNz");
}

#[test]
fn test_bearer_authorization_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .authorization(bearer_token("my-jwt-token"))
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Authorization").unwrap(), "Bearer my-jwt-token");
}

#[test]
fn test_proxy_authorization_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .proxy_authorization(basic_auth("proxyuser", "proxypass"))
        .build()
        .unwrap();

    assert_eq!(
        request.headers.get("Proxy-Authorization").unwrap(),
        "Basic cHJveHl1c2VyOnByb3h5cGFzcw=="
    );
}

// ==================== Connection header tests ====================

#[test]
fn test_connection_close_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .connection_close(true)
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Connection").unwrap(), "close");
}

#[test]
fn test_connection_keep_alive_default() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .build()
        .unwrap();

    // HTTP/1.1 default is keep-alive; no Connection header emitted
    assert!(request.headers.get("Connection").is_none());
}

#[test]
fn test_proxy_keep_alive_header() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .proxy_keep_alive(true)
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Connection").unwrap(), "Keep-Alive");
}

#[test]
fn test_connection_close_overrides_proxy_keep_alive() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .proxy_keep_alive(true)
        .connection_close(true)
        .build()
        .unwrap();

    // connection_close takes priority
    assert_eq!(request.headers.get("Connection").unwrap(), "close");
}

#[test]
fn test_explicit_connection_header_not_overridden() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .header("Connection", "Keep-Alive")
        .connection_close(true)
        .build()
        .unwrap();

    // User-set header takes precedence
    assert_eq!(request.headers.get("Connection").unwrap(), "Keep-Alive");
}

// ==================== Range header tests ====================

#[test]
fn test_range_header_with_end() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .range(1024, Some(2047))
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Range").unwrap(), "bytes=1024-2047");
}

#[test]
fn test_range_header_open_ended() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .range(1024, None)
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Range").unwrap(), "bytes=1024-");
}

#[test]
fn test_range_disabled_by_default() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .build()
        .unwrap();

    assert!(request.headers.get("Range").is_none());
}

// ==================== Combined header tests ====================

#[test]
fn test_multiple_conditional_headers_combined() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .accept_gzip(true)
        .no_cache(true)
        .want_digest(true)
        .if_modified_since("Wed, 21 Oct 2015 07:28:00 GMT".to_string())
        .if_none_match("\"etag-123\"".to_string())
        .referer("http://referrer.example.com/page".to_string())
        .cookie("session=abc".to_string())
        .authorization(basic_auth("user", "pass"))
        .range(0, Some(999))
        .build()
        .unwrap();

    assert_eq!(request.headers.get("Accept-Encoding").unwrap(), "deflate, gzip");
    assert_eq!(request.headers.get("Pragma").unwrap(), "no-cache");
    assert_eq!(request.headers.get("Cache-Control").unwrap(), "no-cache");
    assert_eq!(
        request.headers.get("Want-Digest").unwrap(),
        "SHA-512;q=1, SHA-256;q=1, SHA;q=0.1"
    );
    assert_eq!(
        request.headers.get("If-Modified-Since").unwrap(),
        "Wed, 21 Oct 2015 07:28:00 GMT"
    );
    assert_eq!(request.headers.get("If-None-Match").unwrap(), "\"etag-123\"");
    assert_eq!(
        request.headers.get("Referer").unwrap(),
        "http://referrer.example.com/page"
    );
    assert_eq!(request.headers.get("Cookie").unwrap(), "session=abc");
    assert_eq!(request.headers.get("Authorization").unwrap(), "Basic dXNlcjpwYXNz");
    assert_eq!(request.headers.get("Range").unwrap(), "bytes=0-999");
}

#[test]
fn test_user_header_overrides_auto_generated() {
    let url = Url::parse("http://example.com/file").unwrap();
    let request = HttpRequestBuilder::new(HttpMethod::Get, url)
        .header("Accept-Encoding", "br")
        .accept_gzip(true)
        .header("Want-Digest", "custom-digest")
        .want_digest(true)
        .build()
        .unwrap();

    // User-set headers should take precedence
    assert_eq!(request.headers.get("Accept-Encoding").unwrap(), "br");
    assert_eq!(request.headers.get("Want-Digest").unwrap(), "custom-digest");
}
