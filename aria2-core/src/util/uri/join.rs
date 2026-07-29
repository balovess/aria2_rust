//! URI joining — resolve a (possibly relative) URI against a base URI.
//!
//! Mirrors C++ `uri::joinUri()`. Follows RFC 3986 reference resolution.

use super::construct::construct;
use super::normalize::join_path;
use super::parse::parse;
use super::structs::UriStruct;

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
