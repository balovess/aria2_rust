//! Tests for CookieStorage.

use std::fs;
use std::time::SystemTime;

use super::Cookie;
use super::storage::CookieStorage;

#[test]
fn test_creation_and_count() {
    let store = CookieStorage::new();
    assert_eq!(store.count(), 0);
    assert!(store.is_empty());
}

#[test]
fn test_add_cookie() {
    let store = CookieStorage::new();
    store.add(Cookie::new("sid", "v1", "example.com"));
    assert_eq!(store.count(), 1);
    assert!(!store.is_empty());
}

#[test]
fn test_add_updates_existing() {
    let store = CookieStorage::new();
    store.add(Cookie::new("sid", "old", "example.com"));
    store.add(Cookie::new("sid", "new", "example.com"));
    assert_eq!(store.count(), 1);

    let found = store.find_cookies("example.com", "/", false, false);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].value, "new");
}

#[test]
fn test_find_cookies_filters() {
    let store = CookieStorage::new();
    store.add(Cookie::new("a", "1", "example.com"));
    store.add(Cookie::new("b", "2", "other.com"));

    let found = store.find_cookies("example.com", "/", false, false);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "a");
}

#[test]
fn test_find_cookies_for_url() {
    let store = CookieStorage::new();
    let mut c = Cookie::new("lang", "en", "example.com");
    c.path = "/api".to_string();
    store.add(c);

    let url = reqwest::Url::parse("http://example.com/api/data").unwrap();
    let found = store.find_cookies_for_url(&url);
    assert_eq!(found.len(), 1);

    let url2 = reqwest::Url::parse("http://example.com/home").unwrap();
    let found2 = store.find_cookies_for_url(&url2);
    assert!(found2.is_empty());
}

#[test]
fn test_expire_cookies() {
    let store = CookieStorage::new();
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // "old" cookie expires in 1 second — still in the future so it can be added
    let mut expired = Cookie::new("old", "v", "x.com");
    expired.persistent = true;
    expired.expiry_time = now + 1;
    store.add(expired);

    // "fresh" cookie expires far in the future
    let mut fresh = Cookie::new("fresh", "v", "x.com");
    fresh.persistent = true;
    fresh.expiry_time = i64::MAX;
    store.add(fresh);

    assert_eq!(store.count(), 2);
    // Expire cookies with base_time far in the future, past "old"'s expiry
    let removed = store.expire_cookies(now + 10);
    assert_eq!(removed, 1);
    assert_eq!(store.count(), 1);
}

#[test]
fn test_clear() {
    let store = CookieStorage::new();
    store.add(Cookie::new("a", "1", "b.com"));
    store.add(Cookie::new("c", "2", "d.com"));
    store.clear();
    assert_eq!(store.count(), 0);
}

#[test]
fn test_to_header_string() {
    let store = CookieStorage::new();
    store.add(Cookie::new("a", "1", "example.com"));
    store.add(Cookie::new("b", "2", "example.com"));

    let hdr = store.to_header_string("example.com", "/", false);
    assert!(hdr.contains("a=1"));
    assert!(hdr.contains("b=2"));
}

#[test]
fn test_to_header_string_empty_for_no_match() {
    let store = CookieStorage::new();
    store.add(Cookie::new("a", "1", "example.com"));
    let hdr = store.to_header_string("other.com", "/", false);
    assert!(hdr.is_empty());
}

#[test]
fn test_load_save_roundtrip() {
    let dir = std::env::temp_dir().join("aria2_test_cookie_roundtrip");
    fs::create_dir_all(&dir).ok();
    let path = dir.join("cookies.txt");

    let store = CookieStorage::new();
    let mut c = Cookie::new("sid", "abc", "example.com");
    c.host_only = false; // Domain cookie
    store.add(c);
    store.save_file(&path).expect("save should succeed");

    let store2 = CookieStorage::new();
    let n = store2.load_file(&path).expect("load should succeed");
    assert_eq!(n, 1);

    let found = store2.find_cookies("example.com", "/", false, false);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].value, "abc");

    fs::remove_dir_all(dir).ok();
}

#[test]
fn test_contains() {
    let store = CookieStorage::new();
    let c = Cookie::new("sid", "v1", "example.com");
    store.add(c.clone());
    assert!(store.contains(&c));
    assert!(!store.contains(&Cookie::new("other", "v", "example.com")));
}

#[test]
fn test_parse_and_store_valid() {
    let store = CookieStorage::new();
    let result = store.parse_and_store("k=v; path=/; domain=localhost", "localhost", "/");
    assert!(result, "Valid Set-Cookie should be stored");
    assert_eq!(store.count(), 1);
}

#[test]
fn test_parse_and_store_domain_mismatch() {
    let store = CookieStorage::new();
    // Server at evil.com cannot set cookie for bank.com
    let result = store.parse_and_store("k=v; domain=bank.com", "evil.com", "/");
    assert!(!result, "Domain mismatch must be rejected");
    assert_eq!(store.count(), 0);
}

#[test]
fn test_parse_and_store_delete_cookie() {
    let store = CookieStorage::new();
    // First store a cookie
    store.add(Cookie::new("k", "v", "example.com"));
    assert_eq!(store.count(), 1);
    // Now delete it with Max-Age=0
    let result = store.parse_and_store("k=deleted; Max-Age=0", "example.com", "/");
    assert!(
        !result,
        "Delete cookie should return false per C++ behavior"
    );
    assert_eq!(store.count(), 0);
}

#[test]
fn test_save_file_atomicity() {
    // Verify that save_file writes to temp file then renames
    let dir = std::env::temp_dir().join("aria2_test_cookie_atomic");
    fs::create_dir_all(&dir).ok();
    let path = dir.join("cookies.txt");

    let store = CookieStorage::new();
    store.add(Cookie::new("k", "v", "example.com"));
    store.save_file(&path).expect("save should succeed");

    // The final file should exist
    assert!(path.exists(), "Target file should exist after save");
    // The temp file should NOT exist (renamed to target)
    let temp_path = path.with_extension("tmp");
    assert!(!temp_path.exists(), "Temp file should be renamed away");

    fs::remove_dir_all(dir).ok();
}

#[test]
fn test_multiple_domains_independent() {
    let store = CookieStorage::new();
    store.add(Cookie::new("a", "1", "alpha.com"));
    store.add(Cookie::new("b", "2", "beta.com"));
    store.add(Cookie::new("c", "3", "alpha.com"));

    assert_eq!(store.count(), 3);
    assert_eq!(store.find_cookies("alpha.com", "/", false, false).len(), 2);
    assert_eq!(store.find_cookies("beta.com", "/", false, false).len(), 1);
}
