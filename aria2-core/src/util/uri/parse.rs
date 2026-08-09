//! URI parsing — split a URI string into `UriStruct`.
//!
//! Mirrors C++ `uri::parse()`. Uses the `url` crate internally, then
//! decomposes the path into `dir` + `file` to match C++ semantics.

use url::Url;

use super::percent::percent_decode;
use super::structs::{UriStruct, get_default_port};

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
    result.password = parsed.password().map(percent_decode).unwrap_or_default();

    true
}
