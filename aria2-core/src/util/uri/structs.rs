//! URI struct definition and default port mapping.
//!
//! Mirrors C++ `UriStruct` and `FeatureConfig::getDefaultPort`.

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
