//! Metalink/HTTP parser (RFC 6249 / RFC 5988 / RFC 3230).
//!
//! Parses `Link` headers (RFC 5988) and `Digest` headers (RFC 3230) from HTTP
//! responses to extract alternative download URLs and content verification
//! digests, matching the C++ `MetalinkHttpEntry` and `HttpResponse` parsing
//! logic from the original aria2.

use tracing::debug;

use super::helpers::{
    parse_single_digest, parse_single_link, split_link_entries, split_top_level,
};
use super::types::{MetalinkHttpDigest, MetalinkHttpLink, MetalinkHttpResult, DEFAULT_PRI};
use crate::http::header_processor::HttpResponseHead;

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
                if let Some(ref geo) = link.geo
                    && locs_lower.iter().any(|l| l == geo)
                {
                    // Boost priority: reduce effective pri by DEFAULT_PRI
                    // C++ does r.pri -= 999999 which makes it a very high
                    // priority (low number). In Rust, pri is Option<u64>,
                    // so we need to handle the arithmetic carefully.
                    let current_pri = link.pri.unwrap_or(DEFAULT_PRI);
                    link.pri = Some(current_pri.saturating_sub(DEFAULT_PRI));
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
pub(crate) fn deduplicate_digests(digests: Vec<MetalinkHttpDigest>) -> Vec<MetalinkHttpDigest> {
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
            // Keep only one entry per algorithm (the first).
            // entries is non-empty since we checked is_empty above.
            result.push(
                entries
                    .into_iter()
                    .next()
                    .expect("entries is non-empty: is_empty check above guarantees at least one element"),
            );
        }
        // If inconsistent, discard all entries for this algorithm
        // (matches C++ behavior: conflicting digests are removed entirely)
    }

    result
}
