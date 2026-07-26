//! URI parsing, construction, and resolution utilities.
//!
//! Port of the C++ `uri.h` / `uri.cc` module from aria2. Provides:
//! - `UriStruct`: parsed URI components (protocol, host, dir, file, query, etc.)
//! - `parse()`: split a URI string into `UriStruct`
//! - `construct()`: rebuild a URI string from `UriStruct`
//! - `normalize_path()`: resolve `.` / `..` and collapse duplicate `/`
//! - `join_path()`: combine base + relative path with normalization
//! - `join_uri()`: resolve a (possibly relative) URI against a base URI

use url::Url;

// ---------------------------------------------------------------------------
// Default port mapping (mirrors C++ FeatureConfig::getDefaultPort)
// ---------------------------------------------------------------------------

/// Return the default port for well-known URI schemes.
///
/// Matches C++ `getDefaultPort()`:
/// - http → 80, https → 443, ftp → 21, sftp → 22
/// - Unknown → 0
pub fn get_default_port(protocol: &str) -> u16 {
    match protocol {
        "http" => 80,
        "https" => 443,
        "ftp" => 21,
        "sftp" => 22,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// UriStruct — parsed URI components (mirrors C++ uri::UriStruct)
// ---------------------------------------------------------------------------

/// Parsed components of a URI, mirroring the C++ `UriStruct`.
///
/// Key differences from the C++ struct:
/// - `dir` includes the trailing `/` (e.g. `/path/to/`), matching C++ behavior
///   where `dir` is the path minus the basename.
/// - `file` is the last path segment (basename). Empty when the path ends with `/`.
/// - `query` includes the leading `?` when present (e.g. `?key=val`).
/// - `port` is always filled: explicit from the URI or the scheme default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UriStruct {
    /// Scheme / protocol (e.g. "http", "https", "ftp").
    pub protocol: String,
    /// Hostname (IPv6 stored *without* brackets, e.g. "::1").
    pub host: String,
    /// Directory portion of the path (always ends with `/` if non-empty).
    pub dir: String,
    /// File (basename) portion of the path. Empty when path ends with `/`.
    pub file: String,
    /// Query string including the leading `?`, or empty.
    pub query: String,
    /// Username (percent-decoded), or empty.
    pub username: String,
    /// Password (percent-decoded), or empty.
    pub password: String,
    /// Port number (explicit or scheme default).
    pub port: u16,
    /// Whether the URI contained an explicit password.
    pub has_password: bool,
    /// Whether the host is an IPv6 literal address.
    pub ipv6_literal_address: bool,
}

// ---------------------------------------------------------------------------
// parse — split URI into components
// ---------------------------------------------------------------------------

/// Parse a URI string into `UriStruct`.
///
/// Returns `true` on success. On failure, `result` is in an undefined state.
///
/// Mirrors C++ `uri::parse()`. Uses the `url` crate internally, then
/// decomposes the path into `dir` + `file` to match C++ semantics.
pub fn parse(result: &mut UriStruct, uri: &str) -> bool {
    let parsed = match Url::parse(uri) {
        Ok(u) => u,
        Err(_) => return false,
    };

    result.protocol = parsed.scheme().to_owned();

    // Host extraction — strip brackets for IPv6.
    let ipv6 = matches!(parsed.host(), Some(url::Host::Ipv6(_)));
    result.ipv6_literal_address = ipv6;
    result.host = match parsed.host_str() {
        Some(h) if ipv6 => h
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(h)
            .to_owned(),
        Some(h) => h.to_owned(),
        None => return false,
    };

    // Port — fill default when absent.
    let explicit_port = parsed.port();
    if explicit_port.is_none() {
        let def = get_default_port(&result.protocol);
        if def == 0 {
            return false;
        }
        result.port = def;
    } else {
        result.port = parsed.port_or_known_default().unwrap_or(0);
    }

    // Path → dir + file.
    // C++ splits path into dir (everything before basename) and file (basename).
    let path = parsed.path();
    if path.is_empty() || path == "/" {
        result.dir = "/".to_owned();
        result.file = String::new();
    } else {
        match path.rfind('/') {
            Some(slash_pos) => {
                result.dir = path[..=slash_pos].to_owned();
                result.file = path[slash_pos + 1..].to_owned();
            }
            None => {
                result.dir = String::new();
                result.file = path.to_owned();
            }
        }
    }

    // Query — C++ stores the leading '?'.
    result.query = parsed
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();

    // Username / password — percent-decode to match C++ util::percentDecode.
    result.username = percent_decode(parsed.username());
    result.has_password = parsed.password().is_some();
    result.password = parsed
        .password()
        .map(percent_decode)
        .unwrap_or_default();

    true
}

// ---------------------------------------------------------------------------
// construct — rebuild URI from components
// ---------------------------------------------------------------------------

/// Reconstruct a URI string from `UriStruct`.
///
/// Mirrors C++ `uri::construct()`.
pub fn construct(us: &UriStruct) -> String {
    let mut res = String::with_capacity(64);
    res.push_str(&us.protocol);
    res.push_str("://");

    if !us.username.is_empty() {
        res.push_str(&percent_encode(&us.username));
        if us.has_password {
            res.push(':');
            res.push_str(&percent_encode(&us.password));
        }
        res.push('@');
    }

    if us.ipv6_literal_address {
        res.push('[');
        res.push_str(&us.host);
        res.push(']');
    } else {
        res.push_str(&us.host);
    }

    // Append port only when it differs from the scheme default.
    let def_port = get_default_port(&us.protocol);
    if us.port != 0 && def_port != us.port {
        res.push(':');
        res.push_str(&us.port.to_string());
    }

    res.push_str(&us.dir);
    if us.dir.is_empty() || !us.dir.ends_with('/') {
        res.push('/');
    }

    res.push_str(&us.file);
    res.push_str(&us.query);
    res
}

// ---------------------------------------------------------------------------
// normalizePath — state-machine path normalizer (mirrors C++ exactly)
// ---------------------------------------------------------------------------

/// States for the path-normalization state machine.
///
/// Mirrors the anonymous enum in C++ `uri.cc`:
/// `NPATH_START, NPATH_SLASH, NPATH_SDOT, NPATH_DDOT, NPATH_PATHCOMP`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathState {
    Start,
    Slash,
    SingleDot,
    DoubleDot,
    PathComp,
}

/// Normalize a path by:
/// 1. Removing successive `/` (duplicate slashes).
/// 2. Resolving `.` (current directory) components.
/// 3. Resolving `..` (parent directory) components — excess `..` are discarded.
///
/// The resulting path starts with `/` only if the input starts with `/`.
///
/// Mirrors C++ `uri::normalizePath()` exactly, including the state machine
/// and range-based compaction algorithm.
pub fn normalize_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return String::new();
    }

    let mut state = PathState::Start;
    let mut start_with_slash = false;
    // `range` stores pairs (start, end) of path segments to keep.
    // In C++ this is `std::vector<int>` used in pairs.
    let mut range: Vec<usize> = Vec::with_capacity(32);

    for (i, &b) in bytes.iter().enumerate() {
        let ch = b as char;
        state = match state {
            PathState::Start => match ch {
                '.' => {
                    range.push(i);
                    PathState::SingleDot
                }
                '/' => {
                    start_with_slash = true;
                    PathState::Slash
                }
                _ => {
                    range.push(i);
                    PathState::PathComp
                }
            },
            PathState::Slash => match ch {
                '.' => {
                    range.push(i);
                    PathState::SingleDot
                }
                '/' => {
                    // Drop duplicate '/'.
                    PathState::Slash
                }
                _ => {
                    range.push(i);
                    PathState::PathComp
                }
            },
            PathState::SingleDot => match ch {
                '.' => PathState::DoubleDot,
                '/' => {
                    // Drop path component '.'.
                    range.pop();
                    PathState::Slash
                }
                _ => PathState::PathComp,
            },
            PathState::DoubleDot => match ch {
                '/' => {
                    // Drop previous path component before '..'.
                    for _ in 0..3 {
                        range.pop();
                    }
                    PathState::Slash
                }
                _ => PathState::PathComp,
            },
            PathState::PathComp => {
                if ch == '/' {
                    // Record start of next segment (position after '/').
                    range.push(i + 1);
                    PathState::Slash
                } else {
                    PathState::PathComp
                }
            }
        };
    }

    // Handle end-of-string transitions.
    match state {
        PathState::SingleDot => {
            range.pop();
        }
        PathState::DoubleDot => {
            for _ in 0..3 {
                range.pop();
            }
        }
        PathState::PathComp => {
            range.push(len);
        }
        _ => {}
    }

    // Reconstruct the string from the kept ranges.
    let mut out = Vec::with_capacity(len);
    if start_with_slash {
        out.push(b'/');
    }

    let mut i = 0;
    while i + 1 < range.len() {
        let a = range[i];
        let b = range[i + 1];
        out.extend_from_slice(&bytes[a..b]);
        i += 2;
    }

    String::from_utf8(out).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// joinPath — combine base path with relative path
// ---------------------------------------------------------------------------

/// Join a base path with a new (possibly relative) path, then normalize.
///
/// If `new_path` starts with `/`, it is treated as absolute and `base_path`
/// is ignored (after normalization). Otherwise, `new_path` is appended to
/// `base_path` (with a `/` separator if needed) before normalization.
///
/// Mirrors C++ `uri::joinPath()`.
pub fn join_path(base_path: &str, new_path: &str) -> String {
    join_path_inner(base_path, new_path)
}

fn join_path_inner(base_path: &str, new_path: &str) -> String {
    if new_path.is_empty() {
        return base_path.to_owned();
    }

    // If new_path is absolute or base_path is empty, just normalize new_path.
    if base_path.is_empty() || new_path.starts_with('/') {
        return normalize_path(new_path);
    }

    // Append new_path to base_path.
    let combined = if base_path.ends_with('/') {
        format!("{}{}", base_path, new_path)
    } else {
        format!("{}/{}", base_path, new_path)
    };

    normalize_path(&combined)
}

// ---------------------------------------------------------------------------
// joinUri — resolve a (possibly relative) URI against a base URI
// ---------------------------------------------------------------------------

/// Resolve `uri` against `base_uri`, following RFC 3986 reference resolution.
///
/// - If `uri` is itself an absolute URI (parseable as `UriStruct`), it is
///   returned as-is.
/// - Otherwise, `uri` is treated as a relative reference. Its path portion
///   (up to `?` or `#`) is joined with the base URI's `dir`, then the
///   resulting path replaces the base's path. The query from `uri` (between
///   `?` and `#`) is appended.
///
/// Mirrors C++ `uri::joinUri()`.
pub fn join_uri(base_uri: &str, uri: &str) -> String {
    // If uri is itself an absolute URI, return it unchanged.
    let mut us = UriStruct::default();
    if parse(&mut us, uri) {
        return uri.to_owned();
    }

    // Parse the base URI; if that fails, return uri as-is.
    let mut bus = UriStruct::default();
    if !parse(&mut bus, base_uri) {
        return uri.to_owned();
    }

    // Split uri into path (before '?' or '#') and query (between '?' and '#').
    let qend = uri.find('#').unwrap_or(uri.len());
    let (path_part, query_part) = match uri[..qend].find('?') {
        Some(qpos) => (&uri[..qpos], &uri[qpos..qend]),
        None => (&uri[..qend], &uri[..0]), // empty query slice
    };

    // Join the path with the base URI's directory.
    let new_path = join_path(&bus.dir, path_part);

    // Reconstruct: clear dir/file/query from base, then append new path + query.
    bus.dir.clear();
    bus.file.clear();
    bus.query.clear();
    let mut res = construct(&bus);

    if !new_path.is_empty() {
        // `construct()` always ends with '/'. Since `bus.dir` starts with '/',
        // `new_path` always starts with '/'. Skip the leading '/' to avoid
        // doubling it.
        if let Some(stripped) = new_path.strip_prefix('/') {
            res.push_str(stripped);
        } else {
            res.push_str(&new_path);
        }
    }

    res.push_str(query_part);
    res
}

// ---------------------------------------------------------------------------
// Percent-encoding / decoding helpers
// ---------------------------------------------------------------------------

/// Percent-encode a string for URI components (userinfo, etc.).
///
/// Encodes all bytes that are not in the unreserved set (RFC 3986 §2.3):
/// `ALPHA / DIGIT / "-" / "." / "_" / "~"`.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

/// Percent-decode a string (e.g. `%20` → space, `%E6%96%87` → 文).
///
/// Invalid/incomplete percent sequences are left as-is.
pub fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(
                &String::from_utf8_lossy(&bytes[i + 1..i + 3]),
                16,
            )
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
        assert_eq!(
            construct(&us),
            "http://user:p%40ss@example.com/file.txt"
        );
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
}
