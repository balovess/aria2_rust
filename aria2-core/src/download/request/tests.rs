//! Tests for the `request` module (Request, PeerStat, helpers).

use super::*;
use std::time::Duration;

// ── Construction ─────────────────────────────────────────────────────

#[test]
fn test_new_valid_uri() {
    let req = Request::new("http://example.com/path/file.zip").unwrap();
    assert_eq!(req.uri(), "http://example.com/path/file.zip");
    assert_eq!(req.current_uri(), "http://example.com/path/file.zip");
    assert_eq!(req.protocol(), "http");
    assert_eq!(req.host(), "example.com");
    assert_eq!(req.port(), 80);
    assert_eq!(req.file(), "file.zip");
    assert_eq!(req.dir(), "/path/");
    assert_eq!(req.method(), "GET");
}

#[test]
fn test_new_invalid_uri() {
    assert!(Request::new("not a url :///").is_none());
}

#[test]
fn test_default() {
    let req = Request::default();
    assert_eq!(req.method(), "GET");
    assert_eq!(req.try_count(), 0);
    assert_eq!(req.redirect_count(), 0);
    assert!(req.supports_persistent_connection());
    assert!(!req.keep_alive_hint);
    assert!(!req.pipelining_hint);
    assert_eq!(req.max_pipelined_request(), 1);
    assert!(!req.removal_requested());
    assert_eq!(req.connected_port(), 0);
}

// ── Fragment stripping ───────────────────────────────────────────────

#[test]
fn test_fragment_stripped_on_set_uri() {
    let req = Request::new("http://example.com/file#section").unwrap();
    assert_eq!(req.current_uri(), "http://example.com/file");
    // uri() preserves the original including the fragment
    assert_eq!(req.uri(), "http://example.com/file#section");
}

#[test]
fn test_fragment_stripped_on_referer() {
    let mut req = Request::new("http://example.com/").unwrap();
    req.set_referer("http://example.com/page#anchor");
    assert_eq!(req.referer(), "http://example.com/page");
}

// ── Redirect handling ────────────────────────────────────────────────

#[test]
fn test_redirect_absolute_uri() {
    let mut req = Request::new("http://example.com/old").unwrap();
    assert!(req.redirect_uri("http://other.com/new"));
    assert_eq!(req.current_uri(), "http://other.com/new");
    // Original URI unchanged
    assert_eq!(req.uri(), "http://example.com/old");
    assert_eq!(req.redirect_count(), 1);
}

#[test]
fn test_redirect_relative_uri() {
    let mut req = Request::new("http://example.com/dir/old").unwrap();
    assert!(req.redirect_uri("newfile"));
    assert_eq!(req.current_uri(), "http://example.com/dir/newfile");
    assert_eq!(req.redirect_count(), 1);
}

#[test]
fn test_redirect_protocol_relative_uri() {
    let mut req = Request::new("http://example.com/old").unwrap();
    assert!(req.redirect_uri("//other.com/new"));
    assert_eq!(req.current_uri(), "http://other.com/new");
    assert_eq!(req.redirect_count(), 1);
}

#[test]
fn test_redirect_protocol_relative_preserves_https() {
    let mut req = Request::new("https://example.com/old").unwrap();
    assert!(req.redirect_uri("//other.com/new"));
    assert_eq!(req.current_uri(), "https://other.com/new");
}

#[test]
fn test_redirect_empty_uri_fails() {
    let mut req = Request::new("http://example.com/old").unwrap();
    assert!(!req.redirect_uri(""));
    // redirect_count still incremented (matches C++ behavior)
    assert_eq!(req.redirect_count(), 1);
}

#[test]
fn test_redirect_fragment_stripped() {
    let mut req = Request::new("http://example.com/old").unwrap();
    assert!(req.redirect_uri("http://other.com/new#section"));
    assert_eq!(req.current_uri(), "http://other.com/new");
}

#[test]
fn test_redirect_resets_persistent_connection() {
    let mut req = Request::new("http://example.com/old").unwrap();
    req.set_supports_persistent_connection(false);
    assert!(req.redirect_uri("http://other.com/new"));
    assert!(req.supports_persistent_connection());
}

// ── Reset URI ────────────────────────────────────────────────────────

#[test]
fn test_reset_uri() {
    let mut req = Request::new("http://example.com/original").unwrap();
    req.redirect_uri("http://other.com/redirected");
    assert_eq!(req.current_uri(), "http://other.com/redirected");

    assert!(req.reset_uri());
    assert_eq!(req.current_uri(), "http://example.com/original");
    assert_eq!(req.uri(), "http://example.com/original");
}

#[test]
fn test_reset_uri_clears_connected_addr() {
    let mut req = Request::new("http://example.com/file").unwrap();
    req.set_connected_addr_info("example.com", "93.184.216.34", 80);
    assert!(req.reset_uri());
    assert_eq!(req.connected_hostname(), "");
    assert_eq!(req.connected_addr(), "");
    assert_eq!(req.connected_port(), 0);
}

// ── Try count ────────────────────────────────────────────────────────

#[test]
fn test_try_count() {
    let mut req = Request::new("http://example.com/").unwrap();
    assert_eq!(req.try_count(), 0);
    req.add_try_count();
    req.add_try_count();
    assert_eq!(req.try_count(), 2);
    req.reset_try_count();
    assert_eq!(req.try_count(), 0);
}

// ── Redirect count ───────────────────────────────────────────────────

#[test]
fn test_redirect_count() {
    let mut req = Request::new("http://example.com/").unwrap();
    assert_eq!(req.redirect_count(), 0);
    req.redirect_uri("http://a.com/");
    req.redirect_uri("http://b.com/");
    assert_eq!(req.redirect_count(), 2);
    req.reset_redirect_count();
    assert_eq!(req.redirect_count(), 0);
}

// ── Wake time / resetTryCountAfterWake ───────────────────────────────

#[test]
fn test_wake_time() {
    let mut req = Request::new("http://example.com/").unwrap();
    let future = Instant::now() + Duration::from_secs(60);
    req.set_wake_time(future);
    assert_eq!(req.wake_time(), future);
    assert!(!req.is_wake_time_reached());
}

#[test]
fn test_wake_time_already_passed() {
    let mut req = Request::new("http://example.com/").unwrap();
    let past = Instant::now() - Duration::from_secs(1);
    req.set_wake_time(past);
    assert!(req.is_wake_time_reached());
}

#[test]
fn test_reset_try_count_after_wake() {
    let mut req = Request::new("http://example.com/").unwrap();
    assert!(!req.reset_try_count_after_wake());
    req.set_reset_try_count_after_wake(true);
    assert!(req.reset_try_count_after_wake());
}

// ── Keep-alive and pipelining ────────────────────────────────────────

#[test]
fn test_keep_alive_disabled_by_default() {
    let req = Request::new("http://example.com/").unwrap();
    assert!(!req.is_keep_alive_enabled());
}

#[test]
fn test_keep_alive_enabled_with_hint() {
    let mut req = Request::new("http://example.com/").unwrap();
    req.set_keep_alive_hint(true);
    assert!(req.is_keep_alive_enabled());
}

#[test]
fn test_keep_alive_disabled_when_no_persistent() {
    let mut req = Request::new("http://example.com/").unwrap();
    req.set_keep_alive_hint(true);
    req.set_supports_persistent_connection(false);
    assert!(!req.is_keep_alive_enabled());
}

#[test]
fn test_pipelining_disabled_by_default() {
    let req = Request::new("http://example.com/").unwrap();
    assert!(!req.is_pipelining_enabled());
}

#[test]
fn test_pipelining_enabled_with_hint() {
    let mut req = Request::new("http://example.com/").unwrap();
    req.set_pipelining_hint(true);
    assert!(req.is_pipelining_enabled());
}

#[test]
fn test_pipelining_disabled_when_no_persistent() {
    let mut req = Request::new("http://example.com/").unwrap();
    req.set_pipelining_hint(true);
    req.set_supports_persistent_connection(false);
    assert!(!req.is_pipelining_enabled());
}

#[test]
fn test_pipelining_hint_raw() {
    let mut req = Request::new("http://example.com/").unwrap();
    assert!(!req.is_pipelining_hint());
    req.set_pipelining_hint(true);
    assert!(req.is_pipelining_hint());
}

#[test]
fn test_max_pipelined_request() {
    let mut req = Request::new("http://example.com/").unwrap();
    assert_eq!(req.max_pipelined_request(), 1);
    req.set_max_pipelined_request(5);
    assert_eq!(req.max_pipelined_request(), 5);
}

// ── PeerStat ─────────────────────────────────────────────────────────

#[test]
fn test_peer_stat_none_initially() {
    let req = Request::new("http://example.com/").unwrap();
    assert!(req.peer_stat().is_none());
}

#[test]
fn test_init_peer_stat() {
    let mut req = Request::new("http://example.com/file").unwrap();
    let stat = req.init_peer_stat();
    assert_eq!(stat.cuid, 0);
    assert_eq!(stat.hostname, "example.com");
    assert_eq!(stat.protocol, "http");
}

#[test]
fn test_init_peer_stat_uses_original_uri() {
    let mut req = Request::new("http://original.com/file").unwrap();
    req.redirect_uri("https://redirected.com/other");
    let stat = req.init_peer_stat();
    // PeerStat uses original URI, not redirected
    assert_eq!(stat.hostname, "original.com");
    assert_eq!(stat.protocol, "http");
}

// ── Removal ──────────────────────────────────────────────────────────

#[test]
fn test_removal_flag() {
    let mut req = Request::new("http://example.com/").unwrap();
    assert!(!req.removal_requested());
    req.request_removal();
    assert!(req.removal_requested());
}

// ── Connected address info ───────────────────────────────────────────

#[test]
fn test_connected_addr_info() {
    let mut req = Request::new("http://example.com/").unwrap();
    req.set_connected_addr_info("cdn.example.com", "93.184.216.34", 443);
    assert_eq!(req.connected_hostname(), "cdn.example.com");
    assert_eq!(req.connected_addr(), "93.184.216.34");
    assert_eq!(req.connected_port(), 443);
}

// ── URI component accessors ──────────────────────────────────────────

#[test]
fn test_protocol_and_host() {
    let req = Request::new("https://www.example.com/path").unwrap();
    assert_eq!(req.protocol(), "https");
    assert_eq!(req.host(), "www.example.com");
}

#[test]
fn test_port_explicit() {
    let req = Request::new("http://example.com:8080/path").unwrap();
    assert_eq!(req.port(), 8080);
}

#[test]
fn test_port_default_http() {
    let req = Request::new("http://example.com/path").unwrap();
    assert_eq!(req.port(), 80);
}

#[test]
fn test_port_default_https() {
    let req = Request::new("https://example.com/path").unwrap();
    assert_eq!(req.port(), 443);
}

#[test]
fn test_dir_and_file() {
    let req = Request::new("http://example.com/dir/subdir/file.txt").unwrap();
    assert_eq!(req.dir(), "/dir/subdir/");
    assert_eq!(req.file(), "file.txt");
}

#[test]
fn test_file_default_for_trailing_slash() {
    let req = Request::new("http://example.com/dir/").unwrap();
    assert_eq!(req.file(), DEFAULT_FILE);
}

#[test]
fn test_file_default_for_root() {
    let req = Request::new("http://example.com/").unwrap();
    assert_eq!(req.file(), DEFAULT_FILE);
}

#[test]
fn test_query() {
    let req = Request::new("http://example.com/file?key=value&foo=bar").unwrap();
    assert_eq!(req.query(), "key=value&foo=bar");
}

#[test]
fn test_query_empty() {
    let req = Request::new("http://example.com/file").unwrap();
    assert_eq!(req.query(), "");
}

#[test]
fn test_username_and_password() {
    let req = Request::new("http://user:pass@example.com/file").unwrap();
    assert_eq!(req.username(), "user");
    assert_eq!(req.password(), Some("pass"));
    assert!(req.has_password());
}

#[test]
fn test_no_password() {
    let req = Request::new("http://user@example.com/file").unwrap();
    assert_eq!(req.username(), "user");
    assert!(req.password().is_none());
    assert!(!req.has_password());
}

#[test]
fn test_ipv6_literal_address() {
    let req = Request::new("http://[::1]/path").unwrap();
    assert!(req.is_ipv6_literal_address());
    assert_eq!(req.host(), "::1");
    assert_eq!(req.uri_host(), "[::1]");
}

#[test]
fn test_ipv4_not_ipv6_literal() {
    let req = Request::new("http://example.com/path").unwrap();
    assert!(!req.is_ipv6_literal_address());
    assert_eq!(req.uri_host(), "example.com");
}

// ── Method ───────────────────────────────────────────────────────────

#[test]
fn test_method_get_default() {
    let req = Request::new("http://example.com/").unwrap();
    assert_eq!(req.method(), METHOD_GET);
}

#[test]
fn test_method_head() {
    let mut req = Request::new("http://example.com/").unwrap();
    req.set_method(METHOD_HEAD);
    assert_eq!(req.method(), METHOD_HEAD);
}

// ── Constants ────────────────────────────────────────────────────────

#[test]
fn test_max_redirect_constant() {
    assert_eq!(MAX_REDIRECT, 20);
}

#[test]
fn test_default_file_constant() {
    assert_eq!(DEFAULT_FILE, "index.html");
}

// ── set_uri resets persistent connection ─────────────────────────────

#[test]
fn test_set_uri_resets_persistent_connection() {
    let mut req = Request::new("http://example.com/").unwrap();
    req.set_supports_persistent_connection(false);
    assert!(req.set_uri("http://other.com/"));
    assert!(req.supports_persistent_connection());
}

// ── PeerStat standalone ──────────────────────────────────────────────

#[test]
fn test_peer_stat_standalone() {
    let mut stat = PeerStat::new(42, "example.com".into(), "http".into());
    assert_eq!(stat.cuid, 42);
    assert_eq!(stat.hostname, "example.com");
    assert_eq!(stat.protocol, "http");
    assert_eq!(stat.avg_download_speed(), 0);

    stat.add_session_download_length(1024);
    assert_eq!(stat.session_download_length, 1024);
    stat.add_session_download_length(u64::MAX);
    // Saturating add — should not overflow
    assert_eq!(stat.session_download_length, u64::MAX);
}
