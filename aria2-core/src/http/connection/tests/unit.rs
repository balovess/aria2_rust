//! Unit tests for the HTTP connection module
//!
//! Contains synchronous and async tests that do not require a live test server:
//! config defaults, manager creation, range/redirect parsing, cookie jar,
//! and basic pool query helpers.

use std::collections::HashSet;
use std::time::Duration;

use url::Url;

use crate::http::cookie_storage::{CookieJar, JarCookie};

use super::super::manager::HttpConnectionManager;
use super::super::types::{HttpConfig, HttpResponse};

fn create_test_config() -> HttpConfig {
    HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(100),
        read_timeout: Duration::from_millis(200),
        write_timeout: Duration::from_millis(200),
        idle_timeout: Duration::from_millis(500),
        max_idle_per_host: 4,
    }
}

// ==================== Config & Manager Basics ====================

#[test]
fn test_config_default() {
    let config = HttpConfig::default();
    assert_eq!(config.max_connections, 16);
    assert_eq!(config.connect_timeout, Duration::from_secs(30));
    assert_eq!(config.read_timeout, Duration::from_secs(60));
    assert_eq!(config.write_timeout, Duration::from_secs(60));
    assert_eq!(config.idle_timeout, Duration::from_secs(300));
    assert_eq!(config.max_idle_per_host, 8);
}

#[test]
fn test_manager_creation() {
    let config = create_test_config();
    let manager = HttpConnectionManager::new(&config);

    assert_eq!(manager.max_connections(), 4);
    assert_eq!(manager.active_count(), 0);
    assert_eq!(manager.pool_size(), 0);
}

// ==================== Range Header Parsing ====================

#[test]
fn test_build_range_header() {
    let manager = HttpConnectionManager::new(&Default::default());

    // Full range
    assert_eq!(manager.build_range_header(0, Some(999)), "bytes=0-999");

    // Open-ended range
    assert_eq!(manager.build_range_header(500, None), "bytes=500-");

    // Single byte
    assert_eq!(manager.build_range_header(42, Some(42)), "bytes=42-42");

    // Large values
    assert_eq!(
        manager.build_range_header(u64::MAX - 1, Some(u64::MAX)),
        "bytes=18446744073709551614-18446744073709551615"
    );
}

#[test]
fn test_parse_content_range() {
    let manager = HttpConnectionManager::new(&Default::default());

    // Normal format (known total)
    assert_eq!(
        manager.parse_content_range("bytes 0-499/1000"),
        Some((0, 499, 1000))
    );

    // Normal format (unknown total)
    assert_eq!(
        manager.parse_content_range("bytes 500-999/*"),
        Some((500, 999, u64::MAX))
    );

    // Boundary value
    assert_eq!(manager.parse_content_range("bytes 0-0/1"), Some((0, 0, 1)));

    // Invalid format
    assert_eq!(manager.parse_content_range(""), None);
    assert_eq!(manager.parse_content_range("invalid"), None);
    assert_eq!(manager.parse_content_range("bits 0-99/1000"), None);
    assert_eq!(manager.parse_content_range("bytes 0-499"), None); // Missing /total
    assert_eq!(manager.parse_content_range("bytes abc-def/1000"), None);
}

#[test]
fn test_range_request_build() {
    let manager = HttpConnectionManager::new(&create_test_config());

    assert_eq!(manager.build_range_header(0, Some(999)), "bytes=0-999");
    assert_eq!(manager.build_range_header(500, None), "bytes=500-");
    assert_eq!(manager.build_range_header(42, Some(42)), "bytes=42-42");

    assert_eq!(
        manager.parse_content_range("bytes 0-499/1000"),
        Some((0, 499, 1000))
    );
    assert_eq!(
        manager.parse_content_range("bytes 500-999/*"),
        Some((500, 999, u64::MAX))
    );
    assert_eq!(manager.parse_content_range("invalid"), None);
}

// ==================== Redirect Following ====================

#[test]
fn test_follow_redirects_success() {
    let manager = HttpConnectionManager::new(&Default::default());
    let current_url = Url::parse("http://example.com/old").unwrap();
    let mut chain = HashSet::new();
    chain.insert(current_url.clone());

    let mut response = HttpResponse::new(301, "Moved Permanently".to_string());
    response
        .headers
        .push(("Location".to_string(), "http://example.com/new".to_string()));

    let result = manager.follow_redirects(&response, &current_url, &chain, 1);
    assert!(result.is_ok());
    let new_url = result.unwrap();
    assert!(new_url.as_str().starts_with("http://example.com/new"));
}

#[test]
fn test_follow_redirects_relative_path() {
    let manager = HttpConnectionManager::new(&Default::default());
    let current_url = Url::parse("http://example.com/path/page.html").unwrap();
    let chain = HashSet::new();

    let mut response = HttpResponse::new(302, "Found".to_string());
    response
        .headers
        .push(("Location".to_string(), "../other".to_string()));

    let result = manager.follow_redirects(&response, &current_url, &chain, 1);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().as_str(), "http://example.com/other");
}

#[test]
fn test_follow_redirects_loop_detection() {
    let manager = HttpConnectionManager::new(&Default::default());
    let url_a = Url::parse("http://example.com/a").unwrap();
    let url_b = Url::parse("http://example.com/b").unwrap();

    let mut chain = HashSet::new();
    chain.insert(url_a.clone());
    chain.insert(url_b.clone());

    let mut response = HttpResponse::new(301, "Moved".to_string());
    response
        .headers
        .push(("Location".to_string(), "http://example.com/a".to_string()));

    // Attempt redirect back to a visited URL (circular)
    let result = manager.follow_redirects(&response, &url_b, &chain, 2);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .to_lowercase()
            .contains("circular redirect")
    );
}

#[test]
fn test_follow_redirects_max_exceeded() {
    let manager = HttpConnectionManager::new(&Default::default());
    let current_url = Url::parse("http://example.com/start").unwrap();
    let chain = HashSet::new();

    let mut response = HttpResponse::new(302, "Found".to_string());
    response.headers.push((
        "Location".to_string(),
        "http://example.com/next".to_string(),
    ));

    // Exceed max redirect count
    let result = manager.follow_redirects(&response, &current_url, &chain, 6);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Max redirect"));
}

#[test]
fn test_follow_redirects_non_redirect_response() {
    let manager = HttpConnectionManager::new(&Default::default());
    let current_url = Url::parse("http://example.com/").unwrap();
    let chain = HashSet::new();

    let response = HttpResponse::new(200, "OK".to_string());

    let result = manager.follow_redirects(&response, &current_url, &chain, 0);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Non-redirect"));
}

#[test]
fn test_follow_redirects_missing_location() {
    let manager = HttpConnectionManager::new(&Default::default());
    let current_url = Url::parse("http://example.com/").unwrap();
    let chain = HashSet::new();

    let response = HttpResponse::new(301, "Moved".to_string());

    let result = manager.follow_redirects(&response, &current_url, &chain, 0);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Location"));
}

#[tokio::test]
async fn test_redirect_follow_5_jumps() {
    let manager = HttpConnectionManager::new(&create_test_config());
    let current_url = Url::parse("http://example.com/start").unwrap();
    let mut redirect_chain = HashSet::new();
    redirect_chain.insert(current_url.clone());

    let urls = [
        "http://example.com/page1",
        "http://example.com/page2",
        "http://example.com/page3",
        "http://example.com/page4",
        "http://example.com/final",
    ];

    let mut current = current_url;
    for (i, target) in urls.iter().enumerate() {
        let mut response = HttpResponse::new(302, "Found".to_string());
        response
            .headers
            .push(("Location".to_string(), target.to_string()));

        redirect_chain.insert(current.clone());

        let result = manager.follow_redirects(&response, &current, &redirect_chain, i as u32);
        assert!(
            result.is_ok(),
            "Redirect {} should succeed: {:?}",
            i + 1,
            result.err()
        );

        current = result.unwrap();
    }

    assert!(current.as_str().contains("example.com/final"));
}

#[tokio::test]
async fn test_redirect_loop_detection() {
    let manager = HttpConnectionManager::new(&create_test_config());

    let url_a = Url::parse("http://example.com/a").unwrap();
    let url_b = Url::parse("http://example.com/b").unwrap();
    let url_c = Url::parse("http://example.com/c").unwrap();

    let mut chain = HashSet::new();
    chain.insert(url_a.clone());
    chain.insert(url_b.clone());
    chain.insert(url_c.clone());

    let mut response = HttpResponse::new(301, "Moved".to_string());
    response
        .headers
        .push(("Location".to_string(), "http://example.com/a".to_string()));

    let result = manager.follow_redirects(&response, &url_c, &chain, 3);

    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .to_lowercase()
            .contains("circular redirect")
    );
}

// ==================== Host Extraction & Debug ====================

#[test]
fn test_extract_host() {
    // With port 80
    let url = Url::parse("http://example.com/path").unwrap();
    assert_eq!(HttpConnectionManager::extract_host(&url), "example.com:80");

    // With port 443
    let url = Url::parse("https://example.com:443/path").unwrap();
    assert_eq!(HttpConnectionManager::extract_host(&url), "example.com:443");

    // Custom port
    let url = Url::parse("http://example.com:8080/path").unwrap();
    assert_eq!(
        HttpConnectionManager::extract_host(&url),
        "example.com:8080"
    );
}

#[test]
fn test_debug_format() {
    let config = create_test_config();
    let manager = HttpConnectionManager::new(&config);
    let debug_str = format!("{:?}", manager);

    assert!(debug_str.contains("HttpConnectionManager"));
    assert!(debug_str.contains("max_connections: 4"));
    assert!(debug_str.contains("active_count: 0"));
}

// ==================== Cookie Jar Integration Tests (J4) ====================

#[test]
fn test_cookie_jar_initially_none() {
    let mut manager = HttpConnectionManager::new(&create_test_config());
    assert!(manager.cookie_jar().is_none());
    assert!(manager.cookie_jar_mut().is_none());

    // Attaching cookies without a jar should return None
    let url = Url::parse("https://example.com/").unwrap();
    assert!(manager.attach_cookies_to_request(&url).is_none());
}

#[test]
fn test_set_and_get_cookie_jar() {
    let mut manager = HttpConnectionManager::new(&create_test_config());

    // Initially no jar
    assert!(manager.cookie_jar().is_none());

    // Set a cookie jar
    let jar = CookieJar::new();
    manager.set_cookie_jar(Some(jar));
    assert!(manager.cookie_jar().is_some());

    // Clear it
    manager.set_cookie_jar(None);
    assert!(manager.cookie_jar().is_none());
}

#[test]
fn test_attach_cookies_to_request() {
    let mut manager = HttpConnectionManager::new(&create_test_config());

    // Create jar and add cookies
    let mut jar = CookieJar::new();
    jar.store(JarCookie::new("session_id", "abc123", "example.com"));
    jar.store(JarCookie::new("theme", "dark", "example.com"));
    manager.set_cookie_jar(Some(jar));

    // Attach cookies for example.com URL
    let url = Url::parse("http://example.com/api/data").unwrap();
    let header = manager.attach_cookies_to_request(&url);
    assert!(header.is_some(), "Should return Some with matching cookies");
    let hdr = header.unwrap();
    assert!(
        hdr.contains("session_id=abc123"),
        "Header should contain session_id cookie: {}",
        hdr
    );
    assert!(
        hdr.contains("theme=dark"),
        "Header should contain theme cookie: {}",
        hdr
    );

    // No cookies for different domain
    let url2 = Url::parse("http://other.com/").unwrap();
    let header2 = manager.attach_cookies_to_request(&url2);
    assert!(header2.is_none(), "No cookies should match other domain");
}

#[test]
fn test_extract_cookies_from_response() {
    let mut manager = HttpConnectionManager::new(&create_test_config());
    manager.set_cookie_jar(Some(CookieJar::new()));

    // Simulate response headers with Set-Cookie
    let response_headers = vec![
        (
            "Set-Cookie".to_string(),
            "session=xyz789; Domain=example.com; Path=/".to_string(),
        ),
        (
            "Set-Cookie".to_string(),
            "prefs=en-US; Domain=example.com; Path=/; Secure; HttpOnly".to_string(),
        ),
        ("Content-Type".to_string(), "text/html".to_string()), // Non-cookie header
    ];

    let url = Url::parse("https://example.com/login").unwrap();
    let count = manager.extract_cookies_from_response(&response_headers, &url);

    assert_eq!(count, 2, "Should extract exactly 2 cookies");

    // Verify cookies were stored
    let jar = manager.cookie_jar().as_ref().unwrap();
    assert_eq!(jar.len(), 2, "Jar should contain 2 stored cookies");

    // Verify we can retrieve them
    let cookies = jar.get_cookies_for_url("https://example.com/", true);
    assert_eq!(cookies.len(), 2);

    let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"session"));
    assert!(names.contains(&"prefs"));

    // Verify Secure flag was parsed correctly
    let prefs_cookie = cookies.iter().find(|c| c.name == "prefs").unwrap();
    assert!(prefs_cookie.secure, "prefs cookie should be marked secure");
    assert!(
        prefs_cookie.http_only,
        "prefs cookie should be marked http_only"
    );
}

#[test]
fn test_extract_cookies_no_jar_returns_zero() {
    let mut manager = HttpConnectionManager::new(&create_test_config());
    // No cookie jar set

    let headers = vec![("Set-Cookie".to_string(), "test=val".to_string())];
    let url = Url::parse("http://example.com/").unwrap();
    let count = manager.extract_cookies_from_response(&headers, &url);

    assert_eq!(count, 0, "Should return 0 when no jar is set");
}

#[test]
fn test_extract_cookies_invalid_header_skipped() {
    let mut manager = HttpConnectionManager::new(&create_test_config());
    manager.set_cookie_jar(Some(CookieJar::new()));

    // Mix of valid and invalid Set-Cookie headers
    let headers = vec![
        (
            "Set-Cookie".to_string(),
            "valid=test_value; Domain=x.com".to_string(),
        ),
        ("Set-Cookie".to_string(), "no-equal-sign".to_string()), // Invalid format
        ("Set-Cookie".to_string(), "".to_string()),              // Empty - invalid
    ];

    let url = Url::parse("http://x.com/").unwrap();
    let count = manager.extract_cookies_from_response(&headers, &url);

    assert_eq!(count, 1, "Only 1 valid cookie should be extracted");

    let jar = manager.cookie_jar().as_ref().unwrap();
    assert_eq!(jar.len(), 1);
    let cookies = jar.get_cookies_for_url("http://x.com/", false);
    assert_eq!(cookies[0].name, "valid");
}

#[test]
fn test_debug_format_includes_cookie_jar() {
    let mut manager = HttpConnectionManager::new(&create_test_config());
    let debug_str = format!("{:?}", manager);
    assert!(!debug_str.contains("cookie_jar_set: true"));

    manager.set_cookie_jar(Some(CookieJar::new()));
    let debug_str_with_jar = format!("{:?}", manager);
    assert!(
        debug_str_with_jar.contains("cookie_jar_set: true"),
        "Debug output should show cookie_jar is set: {}",
        debug_str_with_jar
    );
}

#[test]
fn test_secure_cookie_not_sent_over_http() {
    let mut manager = HttpConnectionManager::new(&create_test_config());
    let mut jar = CookieJar::new();

    // Add a secure-only cookie
    let mut secure_cookie = JarCookie::new("token", "secret", "secure.example.com");
    secure_cookie.secure = true;
    jar.store(secure_cookie);

    manager.set_cookie_jar(Some(jar));

    // Over HTTP — should NOT get the secure cookie
    let url_http = Url::parse("http://secure.example.com/api").unwrap();
    let header_http = manager.attach_cookies_to_request(&url_http);
    assert!(
        header_http.is_none(),
        "Secure cookie must not be sent over HTTP"
    );

    // Over HTTPS — SHOULD get the secure cookie
    let url_https = Url::parse("https://secure.example.com/api").unwrap();
    let header_https = manager.attach_cookies_to_request(&url_https);
    assert!(
        header_https.is_some(),
        "Secure cookie should be sent over HTTPS"
    );
    assert!(
        header_https.unwrap().contains("token=secret"),
        "Header should contain the secure token cookie"
    );
}

// ==================== Pool Query Basics ====================

#[test]
fn test_max_idle_per_host_default() {
    let config = HttpConfig::default();
    assert_eq!(config.max_idle_per_host, 8);
}

#[test]
fn test_idle_count_for_key_empty() {
    let config = create_test_config();
    let manager = HttpConnectionManager::new(&config);
    use super::super::active_connection::ConnectionPoolKey;
    let key = ConnectionPoolKey {
        target: "example.com:80".to_string(),
        proxy: None,
    };
    assert_eq!(manager.idle_count_for_key(&key), 0);
}

#[test]
fn test_check_timeout_empty_pool() {
    let mut manager = HttpConnectionManager::new(&create_test_config());
    let evicted = manager.check_timeout();
    assert_eq!(evicted, 0);
}
