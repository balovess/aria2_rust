//! Metalink/HTTP parser (RFC 6249 / RFC 5988 / RFC 3230)
//!
//! Parses `Link` headers (RFC 5988) and `Digest` headers (RFC 3230) from HTTP
//! responses to extract alternative download URLs and content verification
//! digests, matching the C++ `MetalinkHttpEntry` and `HttpResponse` parsing
//! logic from the original aria2.
//!
//! # Link header format (RFC 5988)
//!
//! ```text
//! Link: <http://mirror1>; rel="duplicate"; pri="1",
//!       <http://mirror2>; rel="duplicate"; pri="2"
//! ```
//!
//! # Digest header format (RFC 3230)
//!
//! ```text
//! Digest: sha-256=base64value,md5=base64value
//! ```

use tracing::debug;

use super::header_processor::HttpResponseHead;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default priority when no `pri` parameter is given (matches C++ aria2).
const DEFAULT_PRI: u64 = 999999;
/// Maximum allowed priority value (matches C++ aria2).
const MAX_PRI: u64 = 999999;

// ---------------------------------------------------------------------------
// MetalinkHttpLink
// ---------------------------------------------------------------------------

/// A single link extracted from a `Link` header (RFC 5988).
///
/// Represents an alternative download URL with associated metadata.
/// Only links with `rel="duplicate"` or `rel="mirror"` are considered
/// relevant for Metalink/HTTP purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetalinkHttpLink {
    /// The download URL from the link target.
    pub uri: String,
    /// Relationship types (e.g., "duplicate", "mirror").
    pub rel: Vec<String>,
    /// Priority — lower values are preferred. `None` means lowest priority.
    pub pri: Option<u64>,
    /// Whether this link is preferred (has the `pref` bare parameter).
    pub pref: bool,
    /// Content type (from `type` parameter).
    pub type_: Option<String>,
    /// Language tag (from `hreflang` parameter).
    pub lang: Option<String>,
    /// Geographic location (from `geo` parameter), lowercased.
    pub geo: Option<String>,
}

impl MetalinkHttpLink {
    fn new(uri: String) -> Self {
        Self {
            uri,
            rel: Vec::new(),
            pri: None,
            pref: false,
            type_: None,
            lang: None,
            geo: None,
        }
    }

    /// Returns the effective sort key: pref links come first, then by pri ascending.
    fn sort_key(&self) -> (bool, u64) {
        (!self.pref, self.pri.unwrap_or(DEFAULT_PRI))
    }

    /// Whether this link is relevant for Metalink/HTTP (has "duplicate" or "mirror" rel).
    pub fn is_relevant(&self) -> bool {
        self.rel.iter().any(|r| {
            let r_lower = r.to_lowercase();
            r_lower == "duplicate" || r_lower == "mirror"
        })
    }
}

// ---------------------------------------------------------------------------
// MetalinkHttpDigest
// ---------------------------------------------------------------------------

/// A content digest extracted from a `Digest` header (RFC 3230).
///
/// Per RFC 3230, digest values are base64-encoded. Some implementations
/// use hex encoding; the consumer must handle decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetalinkHttpDigest {
    /// Algorithm name (lowercased), e.g. "sha-256", "sha-512", "md5".
    pub algorithm: String,
    /// Raw digest value (may be base64 or hex encoded).
    pub value: String,
}

// ---------------------------------------------------------------------------
// MetalinkHttpResult
// ---------------------------------------------------------------------------

/// Combined result of parsing `Link` and `Digest` headers from an HTTP response.
#[derive(Debug, Clone, Default)]
pub struct MetalinkHttpResult {
    /// Alternative download URLs, sorted by priority (pref first, then pri ascending).
    pub links: Vec<MetalinkHttpLink>,
    /// Content verification digests.
    pub digests: Vec<MetalinkHttpDigest>,
}

// ---------------------------------------------------------------------------
// MetalinkHttpParser
// ---------------------------------------------------------------------------

/// Parser for Metalink/HTTP information from HTTP response headers.
///
/// All methods are associated functions (no state); use
/// [`MetalinkHttpParser::parse_response`] for the full workflow.
pub struct MetalinkHttpParser;

impl MetalinkHttpParser {
    /// Parse a `Link` header value into a list of relevant Metalink/HTTP links.
    ///
    /// Multiple comma-separated link entries within one header value are handled
    /// per RFC 5988. Only entries with `rel="duplicate"` or `rel="mirror"` are
    /// returned, sorted by priority (pref first, then pri ascending).
    pub fn parse_link_header(header_value: &str) -> Vec<MetalinkHttpLink> {
        let entries = split_link_entries(header_value);
        let mut links: Vec<MetalinkHttpLink> = entries
            .iter()
            .filter_map(|entry| parse_single_link(entry))
            .filter(|link| link.is_relevant())
            .collect();
        links.sort_by_key(|l| l.sort_key());
        links
    }

    /// Parse a `Digest` header value (RFC 3230).
    ///
    /// Format: `algorithm=value,algorithm=value`
    /// The digest value is stored as-is (RFC 3230 specifies base64).
    pub fn parse_digest_header(header_value: &str) -> Vec<MetalinkHttpDigest> {
        let mut digests = Vec::new();
        for param in split_top_level(header_value, ',') {
            let trimmed = param.trim();
            if let Some(digest) = parse_single_digest(trimmed) {
                digests.push(digest);
            }
        }
        digests
    }

    /// Parse `Link` and `Digest` headers from a complete HTTP response.
    ///
    /// Extracts all `Link` header values (there may be multiple), parses them
    /// into `MetalinkHttpLink` entries, and similarly parses **all** `Digest`
    /// header values. Links are sorted by priority. Digests are deduplicated
    /// per algorithm (conflicting values for the same algorithm are removed,
    /// matching C++ `getDigest()` behavior).
    ///
    /// # metalink-location preference
    ///
    /// When `preferred_locations` is non-empty, entries whose `geo` matches
    /// a preferred location get their priority boosted (pri reduced by
    /// `DEFAULT_PRI`), matching C++ `getMetalinKHttpEntries()` which does
    /// `r.pri -= 999999`. This ensures geo-preferred mirrors are tried first.
    pub fn parse_response(
        response: &HttpResponseHead,
        preferred_locations: &[String],
    ) -> MetalinkHttpResult {
        let mut all_links = Vec::new();
        for value in response.header_all("link") {
            all_links.extend(Self::parse_link_header(value));
        }

        // Apply metalink-location preference (matches C++ getMetalinKHttpEntries)
        // C++ code: if (std::find(locs.begin(), locs.end(), r.geo) != locs.end())
        //   { r.pri -= 999999; }
        if !all_links.is_empty() && !preferred_locations.is_empty() {
            // Pre-lowercase preferred locations for comparison
            let locs_lower: Vec<String> =
                preferred_locations.iter().map(|l| l.to_lowercase()).collect();
            for link in &mut all_links {
                if let Some(ref geo) = link.geo {
                    if locs_lower.iter().any(|l| l == geo) {
                        // Boost priority: reduce effective pri by DEFAULT_PRI
                        // C++ does r.pri -= 999999 which makes it a very high
                        // priority (low number). In Rust, pri is Option<u64>,
                        // so we need to handle the arithmetic carefully.
                        let current_pri = link.pri.unwrap_or(DEFAULT_PRI);
                        link.pri = Some(current_pri.saturating_sub(DEFAULT_PRI as u64));
                    }
                }
            }
            // Re-sort after priority adjustment
            all_links.sort_by_key(|l| l.sort_key());
        }

        // Parse ALL Digest header values, not just the first one.
        // Matches C++ getDigest() which iterates equalRange(DIGEST).
        let mut all_digests = Vec::new();
        for digest_value in response.header_all("digest") {
            all_digests.extend(Self::parse_digest_header(digest_value));
        }

        // Deduplicate digests per algorithm (matches C++ getDigest behavior).
        // C++ logic: for each hash type, if multiple entries with different
        // values exist, all entries of that type are removed (inconsistent).
        if !all_digests.is_empty() {
            all_digests = deduplicate_digests(all_digests);
        }

        debug!(
            links = all_links.len(),
            digests = all_digests.len(),
            "Metalink/HTTP parse result"
        );

        MetalinkHttpResult {
            links: all_links,
            digests: all_digests,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal: Digest deduplication (matches C++ getDigest logic)
// ---------------------------------------------------------------------------

/// Deduplicate digests per algorithm.
///
/// Matches C++ `getDigest()` behavior: for each hash algorithm, if multiple
/// entries with different values exist (inconsistent digests), ALL entries
/// for that algorithm are removed. If all entries for the same algorithm
/// have the same value, only one is kept.
fn deduplicate_digests(digests: Vec<MetalinkHttpDigest>) -> Vec<MetalinkHttpDigest> {
    if digests.is_empty() {
        return digests;
    }

    // Group by algorithm
    let mut groups: std::collections::HashMap<String, Vec<MetalinkHttpDigest>> =
        std::collections::HashMap::new();
    for d in digests {
        groups.entry(d.algorithm.clone()).or_default().push(d);
    }

    let mut result = Vec::new();
    for (_, entries) in groups {
        if entries.is_empty() {
            continue;
        }

        // Check if all values for this algorithm are consistent
        let first_value = &entries[0].value;
        let consistent = entries.iter().all(|e| e.value == *first_value);

        if consistent {
            // Keep only one entry per algorithm (the first)
            result.push(entries.into_iter().next().unwrap());
        }
        // If inconsistent, discard all entries for this algorithm
        // (matches C++ behavior: conflicting digests are removed entirely)
    }

    result
}

// ---------------------------------------------------------------------------
// Internal: Link header splitting (handles commas inside quotes/angle brackets)
// ---------------------------------------------------------------------------

/// Split a Link header value into individual link entries by top-level commas.
///
/// Commas inside `""` or `<>` are not treated as delimiters.
fn split_link_entries(header: &str) -> Vec<&str> {
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
// Internal: Single link parsing
// ---------------------------------------------------------------------------

/// Parse one link entry: `<URI>; param=value; ...`
fn parse_single_link(entry: &str) -> Option<MetalinkHttpLink> {
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
// Internal: Digest parsing
// ---------------------------------------------------------------------------

/// Parse a single digest entry: `algorithm=value`.
fn parse_single_digest(param: &str) -> Option<MetalinkHttpDigest> {
    let eq_pos = param.find('=')?;
    let algorithm = param[..eq_pos].trim().to_lowercase();
    let value = unquote(param[eq_pos + 1..].trim());
    if algorithm.is_empty() || value.is_empty() {
        return None;
    }
    Some(MetalinkHttpDigest { algorithm, value })
}

// ---------------------------------------------------------------------------
// Internal: General-purpose helpers
// ---------------------------------------------------------------------------

/// Split a string by a delimiter at the top level, respecting quoted strings.
fn split_top_level(s: &str, delim: char) -> Vec<&str> {
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
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        inner.replace("\\\"", "\"")
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Link header parsing ----

    #[test]
    fn test_simple_link_header() {
        let links =
            MetalinkHttpParser::parse_link_header(r#"<http://mirror1>; rel="duplicate"; pri="1""#);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].uri, "http://mirror1");
        assert_eq!(links[0].rel, vec!["duplicate"]);
        assert_eq!(links[0].pri, Some(1));
    }

    #[test]
    fn test_multiple_links_in_one_header() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://mirror1>; rel="duplicate"; pri="1", <http://mirror2>; rel="mirror"; pri="2""#,
        );
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].uri, "http://mirror1");
        assert_eq!(links[0].pri, Some(1));
        assert_eq!(links[1].uri, "http://mirror2");
        assert_eq!(links[1].pri, Some(2));
    }

    #[test]
    fn test_link_with_all_parameters() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://example.com/file>; rel="duplicate"; pri="3"; type="application/octet-stream"; hreflang="en"; geo="US"; pref"#,
        );
        assert_eq!(links.len(), 1);
        let link = &links[0];
        assert_eq!(link.uri, "http://example.com/file");
        assert_eq!(link.rel, vec!["duplicate"]);
        assert_eq!(link.pri, Some(3));
        assert_eq!(link.type_.as_deref(), Some("application/octet-stream"));
        assert_eq!(link.lang.as_deref(), Some("en"));
        assert_eq!(link.geo.as_deref(), Some("us")); // lowercased
        assert!(link.pref);
    }

    #[test]
    fn test_pref_sorts_first() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://a>; rel="duplicate"; pri="1", <http://b>; rel="duplicate"; pri="5"; pref"#,
        );
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].uri, "http://b"); // pref first
        assert!(links[0].pref);
        assert_eq!(links[1].uri, "http://a");
    }

    #[test]
    fn test_priority_sorting() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://c>; rel="duplicate"; pri="3", <http://a>; rel="duplicate"; pri="1", <http://b>; rel="duplicate"; pri="2""#,
        );
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].uri, "http://a");
        assert_eq!(links[1].uri, "http://b");
        assert_eq!(links[2].uri, "http://c");
    }

    #[test]
    fn test_no_pri_is_lowest_priority() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://no-pri>; rel="duplicate", <http://with-pri>; rel="duplicate"; pri="1""#,
        );
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].uri, "http://with-pri");
        assert_eq!(links[1].uri, "http://no-pri");
        assert_eq!(links[1].pri, None);
    }

    #[test]
    fn test_non_relevant_rel_filtered() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://next>; rel="next", <http://dup>; rel="duplicate""#,
        );
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].uri, "http://dup");
    }

    #[test]
    fn test_mirror_rel_accepted() {
        let links = MetalinkHttpParser::parse_link_header(r#"<http://mirror>; rel="mirror""#);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].rel, vec!["mirror"]);
    }

    #[test]
    fn test_malformed_missing_uri() {
        let links = MetalinkHttpParser::parse_link_header(r#"rel="duplicate"; pri="1""#);
        assert!(links.is_empty());
    }

    #[test]
    fn test_malformed_missing_closing_bracket() {
        let links = MetalinkHttpParser::parse_link_header(r#"<http://incomplete; rel="duplicate""#);
        assert!(links.is_empty());
    }

    #[test]
    fn test_malformed_empty_uri() {
        let links = MetalinkHttpParser::parse_link_header(r#"<   >; rel="duplicate""#);
        assert!(links.is_empty());
    }

    #[test]
    fn test_empty_header() {
        let links = MetalinkHttpParser::parse_link_header("");
        assert!(links.is_empty());
    }

    #[test]
    fn test_comma_inside_quoted_value() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://example.com>; rel="duplicate"; title="hello, world""#,
        );
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].uri, "http://example.com");
    }

    #[test]
    fn test_unquoted_rel() {
        let links = MetalinkHttpParser::parse_link_header(r#"<http://example.com>; rel=duplicate"#);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].rel, vec!["duplicate"]);
    }

    #[test]
    fn test_pri_out_of_range() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://example.com>; rel="duplicate"; pri="0""#,
        );
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].pri, None); // 0 is out of [1, 999999]
    }

    #[test]
    fn test_rel_space_separated() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://example.com>; rel="duplicate mirror""#,
        );
        assert_eq!(links[0].rel, vec!["duplicate", "mirror"]);
        assert!(links[0].is_relevant());
    }

    #[test]
    fn test_geo_lowercased() {
        let links = MetalinkHttpParser::parse_link_header(
            r#"<http://example.com>; rel="duplicate"; geo="US""#,
        );
        assert_eq!(links[0].geo.as_deref(), Some("us"));
    }

    // ---- Digest header parsing ----

    #[test]
    fn test_digest_simple() {
        let digests = MetalinkHttpParser::parse_digest_header("sha-256=abc123,md5=def456");
        assert_eq!(digests.len(), 2);
        assert_eq!(digests[0].algorithm, "sha-256");
        assert_eq!(digests[0].value, "abc123");
        assert_eq!(digests[1].algorithm, "md5");
        assert_eq!(digests[1].value, "def456");
    }

    #[test]
    fn test_digest_quoted_value() {
        let digests = MetalinkHttpParser::parse_digest_header(r#"sha-256="base64value""#);
        assert_eq!(digests.len(), 1);
        assert_eq!(digests[0].algorithm, "sha-256");
        assert_eq!(digests[0].value, "base64value");
    }

    #[test]
    fn test_digest_algorithm_lowercased() {
        let digests = MetalinkHttpParser::parse_digest_header("SHA-256=abc123");
        assert_eq!(digests[0].algorithm, "sha-256");
    }

    #[test]
    fn test_digest_empty_value_skipped() {
        let digests = MetalinkHttpParser::parse_digest_header("sha-256=");
        assert!(digests.is_empty());
    }

    #[test]
    fn test_digest_empty_algorithm_skipped() {
        let digests = MetalinkHttpParser::parse_digest_header("=abc123");
        assert!(digests.is_empty());
    }

    #[test]
    fn test_digest_empty_header() {
        let digests = MetalinkHttpParser::parse_digest_header("");
        assert!(digests.is_empty());
    }

    // ---- Response parsing ----

    #[test]
    fn test_parse_response_with_headers() {
        use super::super::header_processor::HttpHeaderProcessor;

        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\nLink: <http://mirror>; rel=\"duplicate\"; pri=\"1\"\r\nDigest: sha-256=abc123\r\n\r\n");
        let head = proc.get_result().unwrap();

        let result = MetalinkHttpParser::parse_response(&head, &[]);
        assert_eq!(result.links.len(), 1);
        assert_eq!(result.links[0].uri, "http://mirror");
        assert_eq!(result.digests.len(), 1);
        assert_eq!(result.digests[0].algorithm, "sha-256");
    }

    #[test]
    fn test_parse_response_multiple_link_headers() {
        use super::super::header_processor::HttpHeaderProcessor;

        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\nLink: <http://m1>; rel=\"duplicate\"; pri=\"1\"\r\nLink: <http://m2>; rel=\"duplicate\"; pri=\"2\"\r\n\r\n");
        let head = proc.get_result().unwrap();

        let result = MetalinkHttpParser::parse_response(&head, &[]);
        assert_eq!(result.links.len(), 2);
        assert_eq!(result.links[0].uri, "http://m1");
        assert_eq!(result.links[1].uri, "http://m2");
    }

    #[test]
    fn test_parse_response_no_metalink_headers() {
        use super::super::header_processor::HttpHeaderProcessor;

        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n");
        let head = proc.get_result().unwrap();

        let result = MetalinkHttpParser::parse_response(&head, &[]);
        assert!(result.links.is_empty());
        assert!(result.digests.is_empty());
    }

    // ---- metalink-location preference ----

    #[test]
    fn test_metalink_location_preference() {
        use super::super::header_processor::HttpHeaderProcessor;

        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\nLink: <http://us-mirror>; rel=\"duplicate\"; pri=\"1\"; geo=\"us\"\r\nLink: <http://jp-mirror>; rel=\"duplicate\"; pri=\"2\"; geo=\"jp\"\r\n\r\n");
        let head = proc.get_result().unwrap();

        // Without location preference: us-mirror (pri=1) comes first
        let result = MetalinkHttpParser::parse_response(&head, &[]);
        assert_eq!(result.links[0].uri, "http://us-mirror");
        assert_eq!(result.links[1].uri, "http://jp-mirror");

        // With JP location preference: jp-mirror should be boosted
        let result = MetalinkHttpParser::parse_response(&head, &["JP".to_string()]);
        // jp-mirror should now come first (pri boosted from 2 to 2-999999)
        assert_eq!(
            result.links[0].uri, "http://jp-mirror",
            "JP mirror should be first after location preference"
        );
    }

    // ---- Digest deduplication ----

    #[test]
    fn test_digest_deduplication_consistent() {
        let digests = vec![
            MetalinkHttpDigest {
                algorithm: "sha-256".to_string(),
                value: "abc123".to_string(),
            },
            MetalinkHttpDigest {
                algorithm: "sha-256".to_string(),
                value: "abc123".to_string(),
            },
        ];
        let deduped = deduplicate_digests(digests);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].algorithm, "sha-256");
    }

    #[test]
    fn test_digest_deduplication_inconsistent() {
        let digests = vec![
            MetalinkHttpDigest {
                algorithm: "sha-256".to_string(),
                value: "abc123".to_string(),
            },
            MetalinkHttpDigest {
                algorithm: "sha-256".to_string(),
                value: "different".to_string(),
            },
        ];
        let deduped = deduplicate_digests(digests);
        assert!(
            deduped.is_empty(),
            "Inconsistent digests should be removed entirely"
        );
    }

    #[test]
    fn test_digest_deduplication_multiple_algorithms() {
        let digests = vec![
            MetalinkHttpDigest {
                algorithm: "sha-256".to_string(),
                value: "abc123".to_string(),
            },
            MetalinkHttpDigest {
                algorithm: "md5".to_string(),
                value: "def456".to_string(),
            },
        ];
        let deduped = deduplicate_digests(digests);
        assert_eq!(deduped.len(), 2);
    }

    // ---- Multiple Digest headers ----

    #[test]
    fn test_parse_response_multiple_digest_headers() {
        use super::super::header_processor::HttpHeaderProcessor;

        let mut proc = HttpHeaderProcessor::new();
        proc.feed(b"HTTP/1.1 200 OK\r\nDigest: sha-256=abc123\r\nDigest: md5=def456\r\n\r\n");
        let head = proc.get_result().unwrap();

        let result = MetalinkHttpParser::parse_response(&head, &[]);
        assert_eq!(result.digests.len(), 2);
        // Order from HashMap is not guaranteed, so check by content
        let algorithms: Vec<&str> = result.digests.iter().map(|d| d.algorithm.as_str()).collect();
        assert!(algorithms.contains(&"sha-256"));
        assert!(algorithms.contains(&"md5"));
    }

    // ---- Internal helpers ----

    #[test]
    fn test_split_link_entries_respects_quotes() {
        let entries =
            split_link_entries(r#"<http://a>; title="hello, world", <http://b>; rel="duplicate""#);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_unquote_helper() {
        assert_eq!(unquote(r#""hello""#), "hello");
        assert_eq!(unquote(r#""he\"llo""#), "he\"llo");
        assert_eq!(unquote("naked"), "naked");
        assert_eq!(unquote(r#""""#), "");
    }
}
