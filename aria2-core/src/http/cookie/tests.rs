//! Comprehensive tests for cookie module.

use super::Cookie;
use super::parsing::{domain_matches, is_numeric_host, now_secs, path_matches};

#[test]
fn test_creation() {
    let c = Cookie::new("session", "abc123", "example.com");
    assert_eq!(c.name, "session");
    assert_eq!(c.value, "abc123");
    assert_eq!(c.domain, "example.com");
    assert_eq!(c.path, "/");
    assert!(!c.secure);
    assert!(!c.http_only);
    assert!(!c.persistent);
    assert!(!c.is_expired(i64::MAX));
}

#[test]
fn test_match_exact_domain() {
    let mut c = Cookie::new("sid", "v1", "example.com");
    c.host_only = true;
    assert!(c.match_request("example.com", "/", i64::MAX, false, false));
    assert!(!c.match_request("sub.example.com", "/", i64::MAX, false, false));
    assert!(!c.match_request("other.com", "/", i64::MAX, false, false));
}

#[test]
fn test_match_subdomain() {
    let mut c = Cookie::new("sid", "v1", "example.com");
    c.host_only = false;
    assert!(c.match_request("example.com", "/", i64::MAX, false, false));
    assert!(c.match_request("sub.example.com", "/", i64::MAX, false, false));
    assert!(c.match_request("deep.sub.example.com", "/", i64::MAX, false, false));
    assert!(!c.match_request("notexample.com", "/", i64::MAX, false, false));
}

#[test]
fn test_match_secure_flag() {
    let mut c = Cookie::new("token", "t", "api.example.com");
    c.secure = true;
    assert!(c.match_request("api.example.com", "/", i64::MAX, true, false));
    assert!(!c.match_request("api.example.com", "/", i64::MAX, false, false));
}

#[test]
fn test_match_path_prefix_rfc6265() {
    let mut c = Cookie::new("lang", "en", "example.com");

    // path=/api should match /api, /api/, /api/users but NOT /apifoo
    c.path = "/api".to_string();
    assert!(c.match_request("example.com", "/api", i64::MAX, false, false));
    assert!(c.match_request("example.com", "/api/", i64::MAX, false, false));
    assert!(c.match_request("example.com", "/api/users", i64::MAX, false, false));
    assert!(!c.match_request("example.com", "/apifoo", i64::MAX, false, false));
    assert!(!c.match_request("example.com", "/home", i64::MAX, false, false));

    c.path = "/".to_string();
    assert!(c.match_request("example.com", "/any/path", i64::MAX, false, false));
}

#[test]
fn test_path_matches_exact() {
    // Exact path match
    assert!(path_matches("/api", "/api"));
    assert!(path_matches("/", "/"));
}

#[test]
fn test_path_matches_prefix_with_slash() {
    // Cookie path is a prefix and next char is /
    assert!(path_matches("/api", "/api/users"));
    assert!(path_matches("/api", "/api/"));
}

#[test]
fn test_path_matches_prefix_without_slash() {
    // Cookie path is NOT a prefix match if next char is not /
    assert!(!path_matches("/api", "/apifoo"));
    assert!(!path_matches("/api", "/api-bar"));
}

#[test]
fn test_path_matches_trailing_slash() {
    // Cookie path ending with / always matches subpaths
    assert!(path_matches("/api/", "/api/users"));
    assert!(path_matches("/api/", "/api/"));
}

#[test]
fn test_expired_persistent() {
    let mut c = Cookie::new("old", "val", "x.com");
    c.persistent = true;
    c.expiry_time = 1000;
    assert!(c.is_expired(1001));
    assert!(!c.is_expired(999));
    assert!(!c.is_expired(1000));
}

#[test]
fn test_session_never_expires() {
    let mut c = Cookie::new("sess", "v", "x.com");
    c.persistent = false;
    assert!(!c.is_expired(i64::MAX));
}

#[test]
fn test_to_set_cookie_header() {
    let mut c = Cookie::new("session_id", "abc123", "example.com");
    c.path = "/app".to_string();
    let hdr = c.to_set_cookie_header();
    assert!(hdr.starts_with("session_id=abc123"));
    assert!(hdr.contains("Domain=example.com"));
    assert!(hdr.contains("Path=/app"));
}

#[test]
fn test_to_set_cookie_secure_httponly() {
    let mut c = Cookie::new("token", "t", "secure.example.com");
    c.secure = true;
    c.http_only = true;
    let hdr = c.to_set_cookie_header();
    assert!(hdr.contains("Secure"));
    assert!(hdr.contains("HttpOnly"));
}

// ==================== RFC 6265 Set-Cookie parsing ====================

#[test]
fn test_from_set_cookie_basic() {
    // Bare name=value without attributes — previously broken (required ';')
    let hdr = "SID=31d4d96e407aad42";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.name, "SID");
    assert_eq!(c.value, "31d4d96e407aad42");
    assert_eq!(c.domain, "example.com");
    assert!(c.host_only, "No Domain attr → host-only");
    assert_eq!(c.path, "/");
    assert!(!c.secure);
    assert!(!c.http_only);
    assert!(!c.persistent);
}

#[test]
fn test_from_set_cookie_with_attributes() {
    let hdr = "session=xyz; Domain=example.com; Path=/login; Secure; HttpOnly";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.domain, "example.com");
    assert_eq!(c.path, "/login");
    assert!(c.secure);
    assert!(c.http_only);
    assert!(!c.host_only, "Domain attr → subdomain matching");
}

#[test]
fn test_from_set_cookie_leading_dot_domain() {
    // Leading dot in Domain should be stripped
    let hdr = "sid=abc; Domain=.example.com";
    let c = Cookie::from_set_cookie_header(hdr, "www.example.com", "/").unwrap();
    assert_eq!(c.domain, "example.com");
    assert!(!c.host_only);
}

#[test]
fn test_from_set_cookie_domain_validation_rejects_mismatch() {
    // Server at evil.com cannot set cookie for bank.com
    let hdr = "session=hacked; Domain=bank.com";
    assert!(
        Cookie::from_set_cookie_header(hdr, "evil.com", "/").is_none(),
        "Cross-domain cookie injection must be rejected"
    );
}

#[test]
fn test_from_set_cookie_domain_validation_allows_subdomain() {
    // Server at sub.example.com can set cookie for example.com
    let hdr = "sid=abc; Domain=example.com";
    let c = Cookie::from_set_cookie_header(hdr, "sub.example.com", "/").unwrap();
    assert_eq!(c.domain, "example.com");
    assert!(!c.host_only);
}

#[test]
fn test_from_set_cookie_numeric_host_forces_host_only() {
    // Numeric IP hosts cannot receive domain-scoped cookies
    let hdr = "sid=abc; Domain=192.168.1.1";
    let c = Cookie::from_set_cookie_header(hdr, "192.168.1.1", "/").unwrap();
    assert!(c.host_only, "Numeric host must force host-only mode");
    assert_eq!(c.domain, "192.168.1.1");
}

#[test]
fn test_from_set_cookie_empty_domain_rejected() {
    let hdr = "sid=abc; Domain=.";
    assert!(
        Cookie::from_set_cookie_header(hdr, "example.com", "/").is_none(),
        "Empty domain after stripping dot must be rejected"
    );
}

#[test]
fn test_from_set_cookie_max_age_negative() {
    // Max-Age ≤ 0 → cookie should be marked for deletion
    let hdr = "sid=abc; Max-Age=0";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert!(c.persistent);
    assert!(c.is_delete_cookie(), "Max-Age=0 should mark cookie for deletion");
}

#[test]
fn test_from_set_cookie_max_age_positive() {
    let hdr = "sid=abc; Max-Age=3600";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert!(c.persistent);
    assert!(!c.is_delete_cookie());
}

#[test]
fn test_from_set_cookie_max_age_precedence_over_expires() {
    // Max-Age should take precedence over Expires
    let hdr = "sid=abc; Max-Age=3600; Expires=Wed, 09 Jun 2021 10:18:14 GMT";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert!(c.persistent);
    // Max-Age wins, so expiry should be ~3600s from now, not the old date
    assert!(c.expiry_time > now_secs() + 3000);
}

#[test]
fn test_from_set_cookie_value_quote_stripping() {
    let hdr = "sid=\"quoted_value\"";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.value, "quoted_value");
}

#[test]
fn test_from_set_cookie_path_must_start_with_slash() {
    let hdr = "sid=abc; Path=invalid";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/default").unwrap();
    // Invalid path should be ignored → use default_path
    assert_eq!(c.path, "/default");
}

#[test]
fn test_from_set_cookie_empty() {
    assert!(Cookie::from_set_cookie_header("", "x.com", "/").is_none());
    assert!(Cookie::from_set_cookie_header("noequal", "x.com", "/").is_none());
}

// ==================== Domain matching ====================

#[test]
fn test_domain_matches_exact() {
    assert!(domain_matches("example.com", "example.com"));
    assert!(domain_matches("EXAMPLE.COM", "example.com"));
}

#[test]
fn test_domain_matches_subdomain() {
    assert!(domain_matches("sub.example.com", "example.com"));
    assert!(domain_matches("deep.sub.example.com", "example.com"));
}

#[test]
fn test_domain_matches_rejects_partial() {
    // "notexample.com" should NOT match "example.com"
    assert!(!domain_matches("notexample.com", "example.com"));
    // "example.com.evil.com" should NOT match "example.com"
    assert!(!domain_matches("example.com.evil.com", "example.com"));
}

// ==================== Default path computation ====================

#[test]
fn test_default_path_root() {
    assert_eq!(Cookie::default_path("/"), "/");
}

#[test]
fn test_default_path_nested() {
    // Path "/a/b/c" → default is "/a/b" (up to but NOT including the last "/")
    assert_eq!(Cookie::default_path("/a/b/c"), "/a/b");
}

#[test]
fn test_default_path_single_component() {
    // Path "/abc" → only one "/" → default is "/"
    assert_eq!(Cookie::default_path("/abc"), "/");
}

#[test]
fn test_default_path_empty() {
    assert_eq!(Cookie::default_path(""), "/");
}

// ==================== Numeric host detection ====================

#[test]
fn test_is_numeric_host_ipv4() {
    assert!(is_numeric_host("192.168.1.1"));
    assert!(is_numeric_host("127.0.0.1"));
    assert!(!is_numeric_host("example.com"));
}

#[test]
fn test_is_numeric_host_ipv6() {
    assert!(is_numeric_host("::1"));
    assert!(is_numeric_host("[::1]"));
    assert!(is_numeric_host("2001:db8::1"));
}

// ==================== Netscape parsing ====================

#[test]
fn test_parse_netscape_line() {
    let t = "\t";
    let line = [
        ".example.com",
        t,
        "TRUE",
        t,
        "/",
        t,
        "FALSE",
        t,
        "0",
        t,
        "session_id",
        t,
        "abc123",
    ]
    .concat();
    let c = Cookie::parse_netscape_line(&line).unwrap();
    assert_eq!(c.domain, "example.com");
    assert_eq!(c.path, "/");
    assert!(!c.secure);
    assert_eq!(c.name, "session_id");
    assert_eq!(c.value, "abc123");
    // Per C++ NsCookieParser: expiry=0 means session cookie
    assert!(!c.persistent, "expiry=0 should mean session cookie");
}

#[test]
fn test_parse_netscape_skip_comment() {
    assert!(Cookie::parse_netscape_line("# this is a comment").is_none());
    assert!(Cookie::parse_netscape_line("").is_none());
}

#[test]
fn test_parse_netscape_too_few_fields() {
    assert!(Cookie::parse_netscape_line("a\tb\tc").is_none());
}

#[test]
fn test_parse_netscape_secure_true() {
    let t = "\t";
    let line = [
        ".example.com",
        t,
        "TRUE",
        t,
        "/",
        t,
        "TRUE",
        t,
        "0",
        t,
        "token",
        t,
        "secret",
    ]
    .concat();
    let c = Cookie::parse_netscape_line(&line).unwrap();
    assert!(c.secure);
    assert!(!c.persistent, "expiry=0 should mean session cookie");
}

#[test]
fn test_equality_by_name_domain_path() {
    let a = Cookie::new("x", "1", "a.com");
    let b = Cookie::new("x", "2", "a.com");
    assert_eq!(a, b);

    let c = Cookie::new("y", "1", "a.com");
    assert_ne!(a, c);
}

#[test]
fn test_clone() {
    let c = Cookie::new("k", "v", "d.com");
    let c2 = c.clone();
    assert_eq!(c.name, c2.name);
    assert_eq!(c.domain, c2.domain);
}

// ==================== Additional RFC 6265 compliance tests ====================

#[test]
fn test_default_path_no_leading_slash() {
    // RFC 6265: path not starting with / → default is /
    assert_eq!(Cookie::default_path("noslash"), "/");
}

#[test]
fn test_default_path_two_components() {
    // "/a/b" → two slashes → default is "/a" (up to but not including the last /)
    assert_eq!(Cookie::default_path("/a/b"), "/a");
}

#[test]
fn test_default_path_trailing_slash() {
    // "/a/b/" → default is "/a/b" (up to but not including the last /)
    assert_eq!(Cookie::default_path("/a/b/"), "/a/b");
}

#[test]
fn test_host_only_cookie_rejects_subdomain() {
    // Host-only cookies must NOT match subdomain requests
    let mut c = Cookie::new("sid", "v1", "example.com");
    c.host_only = true;
    assert!(c.match_request("example.com", "/", i64::MAX, false, false));
    assert!(
        !c.match_request("sub.example.com", "/", i64::MAX, false, false),
        "Host-only cookie must not match subdomain"
    );
}

#[test]
fn test_domain_cookie_allows_subdomain() {
    // Domain cookies (host_only = false) must match subdomain requests
    let mut c = Cookie::new("sid", "v1", "example.com");
    c.host_only = false;
    assert!(c.match_request("example.com", "/", i64::MAX, false, false));
    assert!(c.match_request("sub.example.com", "/", i64::MAX, false, false));
}

#[test]
fn test_domain_matches_numeric_host_rejects_subdomain() {
    // Numeric hosts must not allow subdomain matching even in domain_matches()
    assert!(domain_matches("192.168.1.1", "192.168.1.1"), "Exact match OK");
    assert!(
        !domain_matches("192.168.1.1", "168.1.1"),
        "Subdomain matching for numeric hosts must be rejected"
    );
}

#[test]
fn test_from_set_cookie_numeric_host_domain_mismatch_rejected() {
    // Server at 192.168.1.1 sets Domain=168.1.1 → must be rejected
    // because domain_matches rejects subdomain matching for numeric hosts
    let hdr = "sid=abc; Domain=168.1.1";
    assert!(
        Cookie::from_set_cookie_header(hdr, "192.168.1.1", "/").is_none(),
        "Numeric host cannot set cookie for subdomain suffix"
    );
}

#[test]
fn test_from_set_cookie_numeric_host_exact_domain_match() {
    // Server at 192.168.1.1 sets Domain=192.168.1.1 → OK but host_only = true
    let hdr = "sid=abc; Domain=192.168.1.1";
    let c = Cookie::from_set_cookie_header(hdr, "192.168.1.1", "/").unwrap();
    assert!(c.host_only, "Numeric host must force host-only even with exact domain match");
    assert_eq!(c.domain, "192.168.1.1");
}

#[test]
fn test_path_matches_cookie_path_without_trailing_slash() {
    // cookie path "/a" should match "/a" and "/a/b" but NOT "/ab"
    assert!(path_matches("/a", "/a"));
    assert!(path_matches("/a", "/a/b"));
    assert!(!path_matches("/a", "/ab"));
    assert!(!path_matches("/a", "/a\\b"));
}

#[test]
fn test_path_matches_root() {
    assert!(path_matches("/", "/"));
    assert!(path_matches("/", "/anything"));
    assert!(path_matches("/", "/a/b/c"));
}

#[test]
fn test_path_matches_with_trailing_slash_in_cookie_path() {
    assert!(path_matches("/a/", "/a/"));
    assert!(path_matches("/a/", "/a/b"));
    assert!(path_matches("/a/", "/a/b/c"));
    assert!(!path_matches("/a/", "/b"));
}

#[test]
fn test_parse_netscape_numeric_host_forces_host_only() {
    let t = "\t";
    let line = [
        "192.168.1.1",
        t,
        "TRUE", // include_subdomains = TRUE, but numeric host → host_only
        t,
        "/",
        t,
        "FALSE",
        t,
        "0",
        t,
        "sid",
        t,
        "abc",
    ]
    .concat();
    let c = Cookie::parse_netscape_line(&line).unwrap();
    assert!(
        c.host_only,
        "Numeric host in Netscape format must force host-only mode"
    );
    assert_eq!(c.domain, "192.168.1.1");
}

#[test]
fn test_from_set_cookie_bare_no_semicolon() {
    // Bug 1: "SID=abc123" without any attributes must parse correctly
    let c = Cookie::from_set_cookie_header("SID=abc123", "example.com", "/").unwrap();
    assert_eq!(c.name, "SID");
    assert_eq!(c.value, "abc123");
    assert!(c.host_only);
    assert!(!c.persistent);
}

#[test]
fn test_from_set_cookie_max_age_negative_value() {
    // Bug 5: Max-Age with negative value -> delete cookie
    let hdr = "sid=abc; Max-Age=-1";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert!(c.persistent);
    assert!(c.is_delete_cookie(), "Negative Max-Age must mark cookie for deletion");
}

#[test]
fn test_from_set_cookie_value_with_quotes() {
    // Bug 6: Double quotes around cookie value must be stripped
    let hdr = "SID=\"abc123\"";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.value, "abc123", "Quotes must be stripped from cookie value");
}

#[test]
fn test_from_set_cookie_value_without_quotes() {
    let hdr = "SID=abc123";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.value, "abc123");
}

#[test]
fn test_from_set_cookie_non_numeric_max_age_rejected() {
    // Per C++ behavior, non-numeric Max-Age should reject the cookie
    let hdr = "sid=abc; Max-Age=invalid";
    assert!(
        Cookie::from_set_cookie_header(hdr, "example.com", "/").is_none(),
        "Non-numeric Max-Age must reject the cookie"
    );
}

// SameSite tests are in tests_samesite.rs
