//! URI construction — rebuild a URI string from `UriStruct`.
//!
//! Mirrors C++ `uri::construct()`.

use super::percent::percent_encode;
use super::structs::{UriStruct, get_default_port};

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
