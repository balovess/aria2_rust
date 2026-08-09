//! Tests for JarCookie and CookieJar.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::jar::{CookieJar, JarCookie};
use super::jar_date::{format_systemtime_as_http_date, parse_http_date};

// ===== JarCookie Tests =====

/// Test J4.4 #1: Parse Set-Cookie header with various attributes.
#[test]
fn test_cookie_parse_set_cookie() {
    let cookie = JarCookie::parse_set_cookie(
        "session_id=abc123; Domain=example.com; Path=/login; Secure; HttpOnly",
    )
    .expect("Should parse valid Set-Cookie header");

    assert_eq!(cookie.name, "session_id");
    assert_eq!(cookie.value, "abc123");
    assert_eq!(cookie.domain, "example.com");
    assert_eq!(cookie.path, "/login");
    assert!(cookie.secure, "Secure flag should be set");
    assert!(cookie.http_only, "HttpOnly flag should be set");
}

/// Test J4.4 #1 continued: Parse minimal Set-Cookie (name=value only).
#[test]
fn test_cookie_parse_minimal() {
    let cookie =
        JarCookie::parse_set_cookie("SID=31d4d96e407aad42").expect("Should parse minimal cookie");

    assert_eq!(cookie.name, "SID");
    assert_eq!(cookie.value, "31d4d96e407aad42");
    assert_eq!(cookie.domain, ""); // No domain specified
    assert_eq!(cookie.path, "/");
    assert!(!cookie.secure);
    assert!(!cookie.http_only);
}

/// Test J4.4 #1 continued: Parse with Max-Age attribute.
#[test]
fn test_cookie_parse_max_age() {
    let cookie =
        JarCookie::parse_set_cookie("token=xyz; Max-Age=3600").expect("Should parse Max-Age");

    assert_eq!(cookie.name, "token");
    assert!(cookie.expires.is_some(), "Max-Age should set expiration");
}

/// Test J4.4 #1 continued: Invalid headers return None.
#[test]
fn test_cookie_parse_invalid_returns_none() {
    assert!(JarCookie::parse_set_cookie("").is_none());
    assert!(JarCookie::parse_set_cookie("noequal").is_none());
    assert!(JarCookie::parse_set_cookie("=").is_none());
    assert!(JarCookie::parse_set_cookie(";").is_none());
}

/// Test J4.4 #2: Secure cookie must not be sent over plain HTTP.
#[test]
fn test_cookie_matches_url_secure_flag() {
    let mut cookie = JarCookie::new("auth_token", "secret123", "secure.example.com");
    cookie.secure = true;

    assert!(
        cookie.matches_url("https://secure.example.com/api", true),
        "Secure cookie should match HTTPS URL"
    );

    assert!(
        !cookie.matches_url("http://secure.example.com/api", false),
        "Secure cookie must NOT match HTTP URL"
    );
}

/// Test J4.4 #2 continued: Non-secure cookies work on both HTTP and HTTPS.
#[test]
fn test_cookie_matches_url_non_secure_both_schemes() {
    let cookie = JarCookie::new("lang", "en", "example.com");

    assert!(
        cookie.matches_url("http://example.com/", false),
        "Non-secure cookie should match HTTP"
    );
    assert!(
        cookie.matches_url("https://example.com/", true),
        "Non-secure cookie should also match HTTPS"
    );
}

/// Test J4.4 #2 continued: Expired cookies don't match.
#[test]
fn test_cookie_matches_url_expired() {
    let mut cookie = JarCookie::new("old_session", "val", "example.com");
    cookie.expires = Some(SystemTime::now() - Duration::from_secs(1));

    assert!(
        !cookie.matches_url("https://example.com/", true),
        "Expired cookie should not match any URL"
    );
}

/// Test J4.4 #3: CookieJar returns correct subset of cookies for a given URL/ URL.
#[test]
fn test_cookie_jar_get_for_url() {
    let mut jar = CookieJar::new();

    jar.store(JarCookie::new("session", "abc123", "example.com"));
    jar.store(JarCookie::new("theme", "dark", "example.com"));
    jar.store(JarCookie::new("tracker", "xyz999", "other.com"));

    let example_cookies = jar.get_cookies_for_url("http://example.com/page", false);
    assert_eq!(
        example_cookies.len(),
        2,
        "Should get exactly 2 cookies for example.com"
    );

    let names: Vec<&str> = example_cookies.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"session"));
    assert!(names.contains(&"theme"));

    let other_cookies = jar.get_cookies_for_url("http://other.com/", false);
    assert_eq!(other_cookies.len(), 1);
    assert_eq!(other_cookies[0].name, "tracker");
}

/// Test J4.4 #3 continued: cookie_header_for_url produces correct header format.
#[test]
fn test_cookie_jar_header_format() {
    let mut jar = CookieJar::new();
    jar.store(JarCookie::new("a", "1", "example.com"));
    jar.store(JarCookie::new("b", "2", "example.com"));

    let header = jar.cookie_header_for_url("http://example.com/", false);
    assert!(header.is_some());
    let hdr = header.unwrap();
    assert!(hdr.contains("a=1"), "Header should contain a=1");
    assert!(hdr.contains("b=2"), "Header should contain b=2");
    assert!(
        hdr == "a=1; b=2" || hdr == "b=2; a=1",
        "Unexpected header format: {}",
        hdr
    );
}

/// Test J4.4 #3 continued: No matching cookies returns None.
#[test]
fn test_cookie_jar_no_match_returns_none() {
    let mut jar = CookieJar::new();
    jar.store(JarCookie::new("x", "y", "example.com"));

    let header = jar.cookie_header_for_url("http://other.com/", false);
    assert!(header.is_none(), "No match should return None");
}

/// Test J4.4 #4: Load cookies from Netscape/Mozilla cookie file format.
#[test]
fn test_netscape_cookie_file_load() {
    let dir = std::env::temp_dir().join("aria2_netscape_test");
    fs::create_dir_all(&dir).ok();
    let path = dir.join("cookies.txt");

    let content = "# Netscape HTTP Cookie File\n\
                   \n\
                   .example.com\tTRUE\t/\tFALSE\t0\tsession_id\tabc123\n\
                   .api.example.com\tTRUE\t/api\tTRUE\t1700000000\ttoken\tsecret\n\
                   localhost\tFALSE\t/\tFALSE\t0\tlocal_key\tlocal_val\n";
    fs::write(&path, content).expect("Failed to write test cookie file");

    let mut jar = CookieJar::new();
    let result = jar.load_netscape_file(&path);
    assert!(
        result.is_ok(),
        "Loading netscape file should succeed: {:?}",
        result.err()
    );

    let count = result.unwrap();
    assert_eq!(count, 3, "Should have loaded 3 cookies");
    assert_eq!(jar.len(), 3);

    let session_cookie = jar
        .cookies
        .iter()
        .find(|c| c.name == "session_id")
        .expect("Should find session_id cookie");
    assert_eq!(session_cookie.domain, ".example.com");
    assert_eq!(session_cookie.value, "abc123");
    assert!(!session_cookie.secure);
    assert!(
        session_cookie.expires.is_none(),
        "Timestamp 0 means session cookie (no expiry)"
    );

    let token_cookie = jar
        .cookies
        .iter()
        .find(|c| c.name == "token")
        .expect("Should find token cookie");
    assert_eq!(token_cookie.domain, ".api.example.com");
    assert_eq!(token_cookie.path, "/api");
    assert!(token_cookie.secure, "Token cookie should be secure");
    assert!(
        token_cookie.expires.is_some(),
        "Token cookie should have expiry time"
    );

    fs::remove_dir_all(dir).ok();
}

/// Test J4.4 #4 continued: Loading nonexistent file returns error.
#[test]
fn test_netscape_load_nonexistent_file() {
    let mut jar = CookieJar::new();
    let result = jar.load_netscape_file(Path::new("/nonexistent/path/cookies.txt"));
    assert!(result.is_err(), "Loading nonexistent file should fail");
}

/// Test J4.4 #4 continued: Empty file loads zero cookies.
#[test]
fn test_netscape_load_empty_file() {
    let dir = std::env::temp_dir().join("aria2_netscape_empty");
    fs::create_dir_all(&dir).ok();
    let path = dir.join("empty.txt");
    fs::write(&path, "").expect("Failed to write empty file");

    let mut jar = CookieJar::new();
    let result = jar.load_netscape_file(&path).expect("Load should succeed");
    assert_eq!(result, 0, "Empty file should yield 0 cookies");
    assert!(jar.is_empty());

    fs::remove_dir_all(dir).ok();
}

// ===== Additional JarCookie/CookieJar tests =====

#[test]
fn test_jar_cookie_creation() {
    let c = JarCookie::new("test", "value", "example.com");
    assert_eq!(c.name, "test");
    assert_eq!(c.value, "value");
    assert_eq!(c.domain, "example.com");
    assert_eq!(c.path, "/");
    assert!(!c.secure);
    assert!(!c.http_only);
    assert!(c.expires.is_none()); // Session cookie by default
}

#[test]
fn test_jar_cookie_to_header_value() {
    let mut c = JarCookie::new("sid", "abc", "example.com");
    c.secure = true;
    c.http_only = true;
    let hdr = c.to_header_value();
    assert!(hdr.starts_with("sid=abc"));
    assert!(hdr.contains("Domain=example.com"));
    assert!(hdr.contains("Secure"));
    assert!(hdr.contains("HttpOnly"));
}

#[test]
fn test_jar_cookie_equality() {
    let a = JarCookie::new("x", "1", "a.com");
    let b = JarCookie::new("x", "2", "a.com"); // Same name+domain+path
    assert_eq!(a, b, "Cookies with same name/domain/path should be equal");

    let c = JarCookie::new("y", "1", "a.com");
    assert_ne!(a, c, "Different names should not be equal");
}

#[test]
fn test_cookie_jar_store_updates_existing() {
    let mut jar = CookieJar::new();
    jar.store(JarCookie::new("sid", "old", "example.com"));
    jar.store(JarCookie::new("sid", "new", "example.com"));

    assert_eq!(jar.len(), 1, "Store should update, not duplicate");
    let cookies = jar.get_cookies_for_url("http://example.com/", false);
    assert_eq!(cookies[0].value, "new");
}

#[test]
fn test_cookie_jar_cleanup_expired() {
    let mut jar = CookieJar::new();

    // Add an expired cookie
    let mut expired = JarCookie::new("old", "val", "x.com");
    expired.expires = Some(SystemTime::now() - Duration::from_secs(60));
    jar.store(expired);

    // Add a fresh cookie (far future expiry)
    let mut fresh = JarCookie::new("fresh", "val", "x.com");
    fresh.expires = Some(SystemTime::now() + Duration::from_secs(86400 * 365));
    jar.store(fresh);

    // Add a session cookie (no expiry)
    jar.store(JarCookie::new("session", "val", "x.com"));

    assert_eq!(jar.len(), 3);
    let removed = jar.cleanup_expired();
    assert_eq!(removed, 1, "Should remove exactly 1 expired cookie");
    assert_eq!(jar.len(), 2, "Fresh + session cookies remain");
}

#[test]
fn test_cookie_jar_clear() {
    let mut jar = CookieJar::new();
    jar.store(JarCookie::new("a", "1", "x.com"));
    jar.store(JarCookie::new("b", "2", "y.com"));
    jar.clear();
    assert!(jar.is_empty());
}

// ===== Date helper tests =====

#[test]
fn test_format_systemtime_as_http_date() {
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(784111777); // Known timestamp
    let formatted = format_systemtime_as_http_date(time);
    // Should produce something like "Thu, 29 Nov 1984 20:22:57 GMT"
    assert!(formatted.contains("GMT"), "HTTP date should end with GMT");
    assert!(
        formatted.contains(','),
        "IMF-fixdate should have comma after weekday"
    );
}

#[test]
fn test_parse_http_date_imf_fixdate() {
    let result = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT");
    assert!(result.is_ok(), "Should parse IMF-fixdate format");
    let time = result.unwrap();
    let dur = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    // Nov 6, 1994 08:49:37 GMT ≈ 784629777 seconds since epoch
    assert!(
        dur.as_secs() > 784000000,
        "Timestamp should be roughly correct, got {}",
        dur.as_secs()
    );
}

#[test]
fn test_parse_http_date_fallback() {
    // Unparseable input should return far-future (not error)
    let result = parse_http_date("totally-invalid-date-string");
    assert!(result.is_ok(), "Should return fallback, not error");
    let time = result.unwrap();
    assert!(time > SystemTime::now(), "Fallback should be in the future");
}
