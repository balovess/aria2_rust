//! SameSite attribute tests for cookie module.

use super::{Cookie, SameSite};

#[test]
fn test_samesite_default_is_none() {
    // Cookie without SameSite attribute defaults to None per C++ compatibility
    let c = Cookie::new("sid", "v", "example.com");
    assert_eq!(c.same_site, SameSite::None);
    assert_eq!(SameSite::default(), SameSite::None);
}

#[test]
fn test_from_set_cookie_samesite_strict() {
    let hdr = "sid=abc; SameSite=Strict";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.same_site, SameSite::Strict);
}

#[test]
fn test_from_set_cookie_samesite_lax() {
    let hdr = "sid=abc; SameSite=Lax";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.same_site, SameSite::Lax);
}

#[test]
fn test_from_set_cookie_samesite_none() {
    // SameSite=None is accepted per C++ aria2 compatibility
    let hdr = "sid=abc; SameSite=None";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.same_site, SameSite::None);
}

#[test]
fn test_from_set_cookie_samesite_none_with_secure() {
    let hdr = "sid=abc; SameSite=None; Secure";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.same_site, SameSite::None);
    assert!(c.secure);
}

#[test]
fn test_from_set_cookie_samesite_case_insensitive() {
    let c1 = Cookie::from_set_cookie_header("foo=bar; SameSite=strict", "example.com", "/").unwrap();
    assert_eq!(c1.same_site, SameSite::Strict);

    let c2 = Cookie::from_set_cookie_header("foo=bar; SameSite=LAX", "example.com", "/").unwrap();
    assert_eq!(c2.same_site, SameSite::Lax);

    let c3 = Cookie::from_set_cookie_header("foo=bar; SameSite=NoNe", "example.com", "/").unwrap();
    assert_eq!(c3.same_site, SameSite::None);
}

#[test]
fn test_from_set_cookie_samesite_unknown_value_defaults_to_none() {
    // Unknown SameSite values are treated as None (the default), per C++ behavior
    let hdr = "sid=abc; SameSite=InvalidValue";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.same_site, SameSite::None, "Unknown SameSite value defaults to None");
}

#[test]
fn test_from_set_cookie_no_samesite_defaults_to_none() {
    // Absent SameSite attribute → default to None per C++ compatibility
    let hdr = "sid=abc";
    let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
    assert_eq!(c.same_site, SameSite::None);
}

#[test]
fn test_samesite_display() {
    assert_eq!(SameSite::None.to_string(), "None");
    assert_eq!(SameSite::Lax.to_string(), "Lax");
    assert_eq!(SameSite::Strict.to_string(), "Strict");
}

// --- SameSite enforcement in match_request() ---

#[test]
fn test_match_request_samesite_strict_same_site() {
    // SameSite=Strict cookies are sent in same-site context
    let mut c = Cookie::new("sid", "v", "example.com");
    c.same_site = SameSite::Strict;
    assert!(c.match_request("example.com", "/", i64::MAX, false, false));
}

#[test]
fn test_match_request_samesite_strict_cross_site_rejected() {
    // SameSite=Strict cookies are NOT sent in cross-site context
    let mut c = Cookie::new("sid", "v", "example.com");
    c.same_site = SameSite::Strict;
    assert!(
        !c.match_request("example.com", "/", i64::MAX, false, true),
        "SameSite=Strict must be rejected in cross-site context"
    );
}

#[test]
fn test_match_request_samesite_lax_same_site() {
    let mut c = Cookie::new("sid", "v", "example.com");
    c.same_site = SameSite::Lax;
    assert!(c.match_request("example.com", "/", i64::MAX, false, false));
}

#[test]
fn test_match_request_samesite_lax_cross_site_allowed() {
    // Lax cookies are allowed on top-level navigations (always for download managers)
    let mut c = Cookie::new("sid", "v", "example.com");
    c.same_site = SameSite::Lax;
    assert!(
        c.match_request("example.com", "/", i64::MAX, false, true),
        "SameSite=Lax should be allowed on top-level navigations"
    );
}

#[test]
fn test_match_request_samesite_none_same_site() {
    let mut c = Cookie::new("sid", "v", "example.com");
    c.same_site = SameSite::None;
    c.secure = true;
    assert!(c.match_request("example.com", "/", i64::MAX, true, false));
}

#[test]
fn test_match_request_samesite_none_cross_site() {
    // SameSite=None cookies are sent in cross-site context (if Secure)
    let mut c = Cookie::new("sid", "v", "example.com");
    c.same_site = SameSite::None;
    c.secure = true;
    assert!(
        c.match_request("example.com", "/", i64::MAX, true, true),
        "SameSite=None should be sent in cross-site context with Secure"
    );
}

#[test]
fn test_match_request_samesite_none_secure_still_enforced() {
    // Secure flag is enforced independently of SameSite
    let mut c = Cookie::new("sid", "v", "example.com");
    c.same_site = SameSite::None;
    c.secure = true;
    // Over HTTPS: OK
    assert!(c.match_request("example.com", "/", i64::MAX, true, false));
    // Over HTTP: rejected by Secure flag
    assert!(
        !c.match_request("example.com", "/", i64::MAX, false, false),
        "Secure cookie must be rejected over HTTP regardless of SameSite"
    );
}

// --- SameSite serialization in Set-Cookie header ---

#[test]
fn test_to_set_cookie_header_samesite_strict() {
    let mut c = Cookie::new("sid", "v", "example.com");
    c.same_site = SameSite::Strict;
    let hdr = c.to_set_cookie_header();
    assert!(hdr.contains("SameSite=Strict"));
}
