//! Tests for URI parsing, construction, normalization, joining, and percent encoding.

use super::*;

// ── get_default_port ────────────────────────────────────────────────

#[test]
fn test_default_port_http() {
    assert_eq!(get_default_port("http"), 80);
}
#[test]
fn test_default_port_https() {
    assert_eq!(get_default_port("https"), 443);
}
#[test]
fn test_default_port_ftp() {
    assert_eq!(get_default_port("ftp"), 21);
}
#[test]
fn test_default_port_sftp() {
    assert_eq!(get_default_port("sftp"), 22);
}
#[test]
fn test_default_port_unknown() {
    assert_eq!(get_default_port("gopher"), 0);
}

// ── parse ──────────────────────────────────────────────────────────

#[test]
fn test_parse_http() {
    let mut us = UriStruct::default();
    assert!(parse(&mut us, "http://example.com/dir/file.txt"));
    assert_eq!(us.protocol, "http");
    assert_eq!(us.host, "example.com");
    assert_eq!(us.dir, "/dir/");
    assert_eq!(us.file, "file.txt");
    assert_eq!(us.query, "");
    assert_eq!(us.port, 80);
    assert!(!us.has_password);
    assert!(!us.ipv6_literal_address);
}

#[test]
fn test_parse_with_query() {
    let mut us = UriStruct::default();
    assert!(parse(&mut us, "http://example.com/file?key=val"));
    assert_eq!(us.query, "?key=val");
}

#[test]
fn test_parse_with_credentials() {
    let mut us = UriStruct::default();
    assert!(parse(&mut us, "http://user:pass@example.com/file"));
    assert_eq!(us.username, "user");
    assert_eq!(us.password, "pass");
    assert!(us.has_password);
}

#[test]
fn test_parse_ipv6() {
    let mut us = UriStruct::default();
    assert!(parse(&mut us, "http://[::1]/path"));
    assert_eq!(us.host, "::1");
    assert!(us.ipv6_literal_address);
}

#[test]
fn test_parse_explicit_port() {
    let mut us = UriStruct::default();
    assert!(parse(&mut us, "http://example.com:8080/file"));
    assert_eq!(us.port, 8080);
}

#[test]
fn test_parse_no_path_defaults_to_slash_dir() {
    let mut us = UriStruct::default();
    assert!(parse(&mut us, "http://example.com"));
    assert_eq!(us.dir, "/");
    assert_eq!(us.file, "");
}

#[test]
fn test_parse_invalid_uri() {
    let mut us = UriStruct::default();
    assert!(!parse(&mut us, "not a url :///"));
}

// ── construct ──────────────────────────────────────────────────────

#[test]
fn test_construct_basic() {
    let us = UriStruct {
        protocol: "http".into(),
        host: "example.com".into(),
        dir: "/dir/".into(),
        file: "file.txt".into(),
        query: String::new(),
        username: String::new(),
        password: String::new(),
        port: 80,
        has_password: false,
        ipv6_literal_address: false,
    };
    assert_eq!(construct(&us), "http://example.com/dir/file.txt");
}

#[test]
fn test_construct_with_credentials() {
    let us = UriStruct {
        protocol: "http".into(),
        host: "example.com".into(),
        dir: "/".into(),
        file: "file.txt".into(),
        query: String::new(),
        username: "user".into(),
        password: "p@ss".into(),
        port: 80,
        has_password: true,
        ipv6_literal_address: false,
    };
    assert_eq!(construct(&us), "http://user:p%40ss@example.com/file.txt");
}

#[test]
fn test_construct_non_default_port() {
    let us = UriStruct {
        protocol: "http".into(),
        host: "example.com".into(),
        dir: "/".into(),
        file: "file.txt".into(),
        query: String::new(),
        username: String::new(),
        password: String::new(),
        port: 8080,
        has_password: false,
        ipv6_literal_address: false,
    };
    assert_eq!(construct(&us), "http://example.com:8080/file.txt");
}

#[test]
fn test_construct_ipv6() {
    let us = UriStruct {
        protocol: "http".into(),
        host: "::1".into(),
        dir: "/path/".into(),
        file: "file.txt".into(),
        query: String::new(),
        username: String::new(),
        password: String::new(),
        port: 80,
        has_password: false,
        ipv6_literal_address: true,
    };
    assert_eq!(construct(&us), "http://[::1]/path/file.txt");
}

#[test]
fn test_construct_with_query() {
    let us = UriStruct {
        protocol: "http".into(),
        host: "example.com".into(),
        dir: "/".into(),
        file: "file.txt".into(),
        query: "?key=val".into(),
        username: String::new(),
        password: String::new(),
        port: 80,
        has_password: false,
        ipv6_literal_address: false,
    };
    assert_eq!(construct(&us), "http://example.com/file.txt?key=val");
}

// ── normalize_path ─────────────────────────────────────────────────

#[test]
fn test_normalize_empty() {
    assert_eq!(normalize_path(""), "");
}

#[test]
fn test_normalize_root() {
    assert_eq!(normalize_path("/"), "/");
}

#[test]
fn test_normalize_simple() {
    assert_eq!(normalize_path("/a/b/c"), "/a/b/c");
}

#[test]
fn test_normalize_duplicate_slashes() {
    assert_eq!(normalize_path("/a//b///c"), "/a/b/c");
}

#[test]
fn test_normalize_current_dir() {
    assert_eq!(normalize_path("/a/./b"), "/a/b");
}

#[test]
fn test_normalize_parent_dir() {
    assert_eq!(normalize_path("/a/b/../c"), "/a/c");
}

#[test]
fn test_normalize_parent_dir_at_root() {
    // Excess '..' at root are discarded.
    assert_eq!(normalize_path("/a/../.."), "/");
}

#[test]
fn test_normalize_trailing_dot() {
    // C++ normalizePath("/a/.") → "/a/"  (the '.' is dropped, but the '/' before it remains)
    assert_eq!(normalize_path("/a/."), "/a/");
}

#[test]
fn test_normalize_trailing_dotdot() {
    // C++ normalizePath("/a/b/..") → "/a/"  (the 'b/..' is dropped, but the '/' before 'b' remains)
    assert_eq!(normalize_path("/a/b/.."), "/a/");
}

#[test]
fn test_normalize_relative_path() {
    // Paths not starting with '/' stay relative.
    assert_eq!(normalize_path("a/b/c"), "a/b/c");
}

#[test]
fn test_normalize_complex() {
    assert_eq!(normalize_path("/a/b/./c/../d//e"), "/a/b/d/e");
}

#[test]
fn test_normalize_dot_only() {
    assert_eq!(normalize_path("."), "");
}

#[test]
fn test_normalize_dotdot_only() {
    assert_eq!(normalize_path(".."), "");
}

#[test]
fn test_normalize_multiple_parent_at_start() {
    // C++ behavior: excess '..' are discarded when there's nothing to go up.
    assert_eq!(normalize_path("../../a"), "a");
}

#[test]
fn test_normalize_slash_dot_slash() {
    assert_eq!(normalize_path("/./"), "/");
}

#[test]
fn test_normalize_slash_dotdot_slash() {
    assert_eq!(normalize_path("/../"), "/");
}

// ── join_path ──────────────────────────────────────────────────────

#[test]
fn test_join_path_empty_new() {
    assert_eq!(join_path("/a/b", ""), "/a/b");
}

#[test]
fn test_join_path_absolute_new() {
    assert_eq!(join_path("/a/b", "/x/y"), "/x/y");
}

#[test]
fn test_join_path_relative() {
    assert_eq!(join_path("/a/b/", "c"), "/a/b/c");
}

#[test]
fn test_join_path_relative_no_trailing_slash() {
    assert_eq!(join_path("/a/b", "c"), "/a/b/c");
}

#[test]
fn test_join_path_empty_base() {
    assert_eq!(join_path("", "/x/y"), "/x/y");
}

#[test]
fn test_join_path_with_dot() {
    assert_eq!(join_path("/a/b", "./c"), "/a/b/c");
}

#[test]
fn test_join_path_with_parent() {
    assert_eq!(join_path("/a/b", "../c"), "/a/c");
}

// ── join_uri ───────────────────────────────────────────────────────

#[test]
fn test_join_uri_absolute_returns_uri() {
    assert_eq!(
        join_uri("http://example.com/a", "http://other.com/b"),
        "http://other.com/b"
    );
}

#[test]
fn test_join_uri_relative_path() {
    let result = join_uri("http://example.com/dir/file", "other");
    assert_eq!(result, "http://example.com/dir/other");
}

#[test]
fn test_join_uri_absolute_path() {
    let result = join_uri("http://example.com/dir/file", "/new/path");
    assert_eq!(result, "http://example.com/new/path");
}

#[test]
fn test_join_uri_with_query() {
    let result = join_uri("http://example.com/dir/file", "other?key=val");
    assert_eq!(result, "http://example.com/dir/other?key=val");
}

#[test]
fn test_join_uri_with_fragment() {
    // Fragment is the endpoint for query extraction — query part goes up to '#'.
    let result = join_uri("http://example.com/dir/file", "other?key=val#anchor");
    assert_eq!(result, "http://example.com/dir/other?key=val");
}

#[test]
fn test_join_uri_invalid_base_returns_uri() {
    assert_eq!(join_uri("not-a-url", "something"), "something");
}

#[test]
fn test_join_uri_relative_parent() {
    let result = join_uri("http://example.com/a/b/file", "../c");
    assert_eq!(result, "http://example.com/a/c");
}

// ── percent_encode / percent_decode ────────────────────────────────

#[test]
fn test_percent_encode_basic() {
    assert_eq!(percent_encode("hello world"), "hello%20world");
}

#[test]
fn test_percent_encode_unreserved() {
    assert_eq!(percent_encode("abcABC123-._~"), "abcABC123-._~");
}

#[test]
fn test_percent_decode_basic() {
    assert_eq!(percent_decode("hello%20world"), "hello world");
}

#[test]
fn test_percent_decode_invalid() {
    assert_eq!(percent_decode("foo%ZZbar"), "foo%ZZbar");
}

#[test]
fn test_percent_decode_incomplete() {
    assert_eq!(percent_decode("foo%2"), "foo%2");
}

#[test]
fn test_roundtrip() {
    let original = "user@host:p@ss word!";
    assert_eq!(percent_decode(&percent_encode(original)), original);
}
