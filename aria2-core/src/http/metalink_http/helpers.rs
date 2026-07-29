//! Internal helper functions for Metalink/HTTP parsing.
//!
//! Contains low-level string splitting, link parameter parsing,
//! digest entry parsing, and quote/escape handling utilities.

use super::types::{MetalinkHttpDigest, MetalinkHttpLink, MAX_PRI};

// ---------------------------------------------------------------------------
// Link header splitting (handles commas inside quotes/angle brackets)
// ---------------------------------------------------------------------------

/// Split a Link header value into individual link entries by top-level commas.
///
/// Commas inside `""` or `<>` are not treated as delimiters.
pub(crate) fn split_link_entries(header: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut depth_angle = 0u8;
    let mut in_quotes = false;
    let mut escape_next = false;

    for (i, ch) in header.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escape_next = true,
            '"' => in_quotes = !in_quotes,
            '<' if !in_quotes => depth_angle = depth_angle.saturating_add(1),
            '>' if !in_quotes => depth_angle = depth_angle.saturating_sub(1),
            ',' if !in_quotes && depth_angle == 0 => {
                let entry = header[start..i].trim();
                if !entry.is_empty() {
                    entries.push(entry);
                }
                start = i + ','.len_utf8();
            }
            _ => {}
        }
    }

    let entry = header[start..].trim();
    if !entry.is_empty() {
        entries.push(entry);
    }
    entries
}

// ---------------------------------------------------------------------------
// Single link parsing
// ---------------------------------------------------------------------------

/// Parse one link entry: `<URI>; param=value; ...`
pub(crate) fn parse_single_link(entry: &str) -> Option<MetalinkHttpLink> {
    let uri_start = entry.find('<')?;
    let uri_end = entry.find('>')?;
    if uri_end <= uri_start {
        return None;
    }
    let uri = entry[uri_start + 1..uri_end].trim().to_string();
    if uri.is_empty() {
        return None;
    }

    let mut link = MetalinkHttpLink::new(uri);
    let rest = &entry[uri_end + 1..];

    if let Some(semi_pos) = rest.find(';') {
        for param in split_top_level(&rest[semi_pos + 1..], ';') {
            parse_link_param(param.trim(), &mut link);
        }
    }
    Some(link)
}

/// Parse a single link parameter (`name=value` or bare `name`).
fn parse_link_param(param: &str, link: &mut MetalinkHttpLink) {
    if param.is_empty() {
        return;
    }
    let (name, value) = match param.find('=') {
        Some(pos) => {
            let n = param[..pos].trim().to_lowercase();
            let v = unquote(param[pos + 1..].trim());
            (n, v)
        }
        None => (param.trim().to_lowercase(), String::new()),
    };

    if name.is_empty() {
        return;
    }

    match name.as_str() {
        "rel" => {
            link.rel = value.split_whitespace().map(|s| s.to_string()).collect();
        }
        "pri" => {
            if let Ok(p) = value.parse::<u64>()
                && (1..=MAX_PRI).contains(&p) {
                    link.pri = Some(p);
                }
        }
        "pref" => {
            link.pref = true;
        }
        "type" => {
            link.type_ = Some(value);
        }
        "hreflang" => {
            link.lang = Some(value);
        }
        "geo" => {
            link.geo = Some(value.to_lowercase());
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Digest parsing
// ---------------------------------------------------------------------------

/// Parse a single digest entry: `algorithm=value`.
pub(crate) fn parse_single_digest(param: &str) -> Option<MetalinkHttpDigest> {
    let eq_pos = param.find('=')?;
    let algorithm = param[..eq_pos].trim().to_lowercase();
    let value = unquote(param[eq_pos + 1..].trim());
    if algorithm.is_empty() || value.is_empty() {
        return None;
    }
    Some(MetalinkHttpDigest { algorithm, value })
}

// ---------------------------------------------------------------------------
// General-purpose helpers
// ---------------------------------------------------------------------------

/// Split a string by a delimiter at the top level, respecting quoted strings.
pub(crate) fn split_top_level(s: &str, delim: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escape_next = false;

    for (i, ch) in s.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escape_next = true,
            '"' => in_quotes = !in_quotes,
            c if c == delim && !in_quotes => {
                parts.push(&s[start..i]);
                start = i + delim.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Remove surrounding double quotes and unescape `\"`.
pub(crate) fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        inner.replace("\\\"", "\"")
    } else {
        s.to_string()
    }
}
