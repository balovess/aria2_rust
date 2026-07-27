//! Feedback-based URI selector matching C++ aria2's `FeedbackURISelector`.
//!
//! This is the default URI selector in C++ aria2, implementing a two-phase
//! selection strategy:
//!
//! 1. **selectFaster** — Pick the fastest available server (speed > 20 KB/s)
//!    that is not already in use. Falls back to untested/normal servers.
//! 2. **selectRarer** — If no fast server is found, prefer servers already
//!    being used by other connections (proven to work).
//!
//! # C++ Reference
//!
//! Based on `FeedbackURISelector.h` / `FeedbackURISelector.cc` from both the
//! original aria2 and aria2-next. The aria2-next version changes `A2_LOG_DEBUG`
//! to `A2_LOG_TRACE` but the algorithm is identical.

use std::sync::Arc;

use tracing::trace;

use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

// ---------------------------------------------------------------------------
// Constants (matching C++ aria2)
// ---------------------------------------------------------------------------

/// Maximum number of "good" URIs to consider in selectFaster().
///
/// From C++: `constexpr size_t NUM_URI = 10;`
/// This introduces some randomness by not considering all URIs.
const NUM_URI: usize = 10;

/// Speed threshold for "fast" server classification (20 KB/s).
///
/// From C++: `constexpr int SPEED_THRESHOLD = 20_k;` (where `20_k = 20480`)
/// Servers with download speed above this threshold are considered "fast".
const SPEED_THRESHOLD: u64 = 20 * 1024; // 20480 bytes/sec

// ---------------------------------------------------------------------------
// URI parsing utilities
// ---------------------------------------------------------------------------

/// Extracts (host, protocol) from a URI string.
///
/// This is the Rust equivalent of the C++ `uri_split()` + `getFieldString()`
/// combination used in `FeedbackURISelector` to extract `USR_HOST` and
/// `USR_SCHEME`.
///
/// # Returns
///
/// - `Some((host, protocol))` on success
/// - `None` if the URI cannot be parsed (missing scheme, empty host)
///
/// # Examples
///
/// ```
/// use aria2_core::selector::feedback_uri_selector::extract_host_and_protocol;
///
/// let (host, proto) = extract_host_and_protocol("http://example.com/path").unwrap();
/// assert_eq!(host, "example.com");
/// assert_eq!(proto, "http");
///
/// let (host, proto) = extract_host_and_protocol("https://cdn.example.com:8443/file").unwrap();
/// assert_eq!(host, "cdn.example.com:8443");
/// assert_eq!(proto, "https");
/// ```
pub fn extract_host_and_protocol(uri: &str) -> Option<(String, String)> {
    let uri = uri.trim();
    let scheme_end = uri.find("://")?;
    let protocol = &uri[..scheme_end];
    let after_scheme = &uri[scheme_end + 3..];
    let host_part = if let Some(slash_idx) = after_scheme.find('/') {
        &after_scheme[..slash_idx]
    } else {
        after_scheme
    };
    if host_part.is_empty() || protocol.is_empty() {
        return None;
    }
    Some((host_part.to_string(), protocol.to_string()))
}

// ---------------------------------------------------------------------------
// FeedbackUriSelector
// ---------------------------------------------------------------------------

/// URI selector using feedback from server statistics.
///
/// This implements the C++ `FeedbackURISelector` algorithm exactly:
///
/// 1. Try `select_faster()` — pick the fastest unused server
/// 2. If nothing found, fall back to `select_rarer()` — prefer proven servers
///
/// # Key Differences from AdaptiveUriSelector
///
/// - Uses `getDownloadSpeed()` (current speed) for selection, not avg speed
/// - Has a 20 KB/s speed threshold for "fast" classification
/// - Limits to 10 candidate URIs in selectFaster() for randomness
/// - selectRarer() prefers servers already in use (proven servers)
/// - Uses protocol-aware server stat lookups (host, protocol)
pub struct FeedbackUriSelector {
    stat_man: Arc<ServerStatMan>,
}

impl FeedbackUriSelector {
    /// Creates a new `FeedbackUriSelector` with the given server statistics manager.
    ///
    /// This matches the C++ constructor:
    /// `FeedbackURISelector(const shared_ptr<ServerStatMan>& serverStatMan)`
    pub fn new(stat_man: Arc<ServerStatMan>) -> Self {
        Self { stat_man }
    }

    /// Select the next URI using the feedback algorithm.
    ///
    /// Matches C++ `FeedbackURISelector::select()`.
    ///
    /// # Algorithm
    ///
    /// 1. If URIs list is empty → return `None`
    /// 2. Try `select_faster()` → find fastest unused server
    /// 3. If no fast server found → try `select_rarer()` → prefer proven servers
    /// 4. Return the index of the selected URI
    fn select_one(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize> {
        if uris.is_empty() {
            return None;
        }

        // Log used hosts (matching C++ DEBUG/TRACE logging)
        for (cuid, host) in used_hosts {
            trace!(cuid, host = %host, "FeedbackURISelector: UsedHost");
        }

        // Try selectFaster first
        if let Some(idx) = self.select_faster(uris, used_hosts) {
            trace!(uri = %uris[idx], "FeedbackURISelector selected (faster)");
            return Some(idx);
        }

        // Fall back to selectRarer
        trace!("No URI returned from selectFaster()");
        if let Some(idx) = self.select_rarer(uris, used_hosts) {
            trace!(uri = %uris[idx], "FeedbackURISelector selected (rarer)");
            return Some(idx);
        }

        trace!("FeedbackURISelector: no URI selected");
        None
    }

    /// Select the fastest available URI.
    ///
    /// Matches C++ `FeedbackURISelector::selectFaster()`.
    ///
    /// # Algorithm
    ///
    /// 1. Consider up to `NUM_URI` (10) URIs
    /// 2. For each URI:
    ///    - Skip if host is already in `used_hosts`
    ///    - If no ServerStat exists → add to `norm_cands` (untested)
    ///    - If ServerStat is OK and speed > 20 KB/s → add to `fast_cands`
    ///    - If ServerStat is OK but speed <= 20 KB/s → add to `norm_cands`
    ///    - If ServerStat is ERROR → skip entirely
    /// 3. If `fast_cands` is not empty → sort by speed, return fastest
    /// 4. If `norm_cands` is not empty → return first
    /// 5. Otherwise → return `None`
    fn select_faster(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize> {
        // Build a set of used host names for O(1) lookup
        let used_set: std::collections::HashSet<&str> =
            used_hosts.iter().map(|(_, h)| h.as_str()).collect();

        // Collect fast and normal candidates
        let mut fast_cands: Vec<(u64, usize)> = Vec::new(); // (speed, index)
        let mut norm_cands: Vec<usize> = Vec::new();

        for (idx, uri) in uris.iter().enumerate() {
            // Limit to NUM_URI candidates (matching C++ behavior)
            if fast_cands.len() >= NUM_URI {
                break;
            }

            let (host, protocol) = match extract_host_and_protocol(uri) {
                Some(hp) => hp,
                None => continue, // URI parse failed, skip
            };

            // Skip if host is already in use
            if used_set.contains(host.as_str()) {
                trace!(uri = %uri, "is in usedHosts, not considered");
                continue;
            }

            // Look up server stat by (host, protocol)
            let stat = self.stat_man.find_stat_by_protocol(&host, &protocol);

            match stat {
                None => {
                    // No stat → untested server, add to normal candidates
                    norm_cands.push(idx);
                }
                Some(s) if s.is_ok() => {
                    if s.get_download_speed() > SPEED_THRESHOLD {
                        // Fast server
                        fast_cands.push((s.get_download_speed(), idx));
                    } else {
                        // Slow or untested server
                        norm_cands.push(idx);
                    }
                }
                Some(_) => {
                    // ServerStat is in ERROR state → skip entirely
                    trace!(uri = %uri, "Error server not considered");
                }
            }
        }

        if fast_cands.is_empty() {
            if norm_cands.is_empty() {
                None
            } else {
                trace!("Selected from normCands");
                Some(norm_cands[0])
            }
        } else {
            trace!("Selected from fastCands");
            // Sort by speed descending (matching C++ ServerStatFaster comparator)
            fast_cands.sort_by_key(|b| std::cmp::Reverse(b.0));
            Some(fast_cands[0].1)
        }
    }

    /// Select a URI preferring hosts already in use (proven servers).
    ///
    /// Matches C++ `FeedbackURISelector::selectRarer()`.
    ///
    /// # Algorithm
    ///
    /// 1. For each URI:
    ///    - Parse URI, skip if invalid
    ///    - If ServerStat exists and is ERROR → skip
    ///    - Otherwise add to candidates as (host, uri_index)
    /// 2. For each usedHost:
    ///    - Check if any candidate's host matches
    ///    - Return the first matching URI index
    /// 3. If no match → return first candidate URI (or first URI if no candidates)
    fn select_rarer(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize> {
        // Build candidates: (host, uri_index)
        let mut cands: Vec<(String, usize)> = Vec::new();

        for (idx, uri) in uris.iter().enumerate() {
            let (host, protocol) = match extract_host_and_protocol(uri) {
                Some(hp) => hp,
                None => continue,
            };

            // Check server stat — skip if ERROR
            if let Some(stat) = self.stat_man.find_stat_by_protocol(&host, &protocol)
                && !stat.is_ok() {
                    trace!(uri = %uri, "Error not considered (rarer)");
                    continue;
                }

            cands.push((host, idx));
        }

        // Prefer URIs whose hosts are already in usedHosts (proven servers)
        for (_, used_host) in used_hosts {
            for (cand_host, cand_idx) in &cands {
                if cand_host == used_host {
                    return Some(*cand_idx);
                }
            }
        }

        // No match with usedHosts → return first candidate
        // Matching C++: `assert(!uris.empty()); return uris.front();`
        cands.first().map(|(_, idx)| *idx).or({
            // If no valid candidates but URIs exist, return first
            if !uris.is_empty() { Some(0) } else { None }
        })
    }
}

impl UriSelector for FeedbackUriSelector {
    fn select(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize> {
        self.select_one(uris, used_hosts)
    }

    fn tune_command(&self, _uris: &[String], _speed: u64) {
        // FeedbackURISelector has no tune_command logic (matching C++)
    }

    fn reset(&self) {
        // FeedbackURISelector has no reset logic (matching C++)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_host_and_protocol_http() {
        let (host, proto) = extract_host_and_protocol("http://example.com/path").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(proto, "http");
    }

    #[test]
    fn test_extract_host_and_protocol_invalid() {
        assert!(extract_host_and_protocol("not-a-uri").is_none());
        assert!(extract_host_and_protocol("").is_none());
    }
}

// Extended tests extracted to separate file to keep this file under 600 lines.
#[cfg(test)]
mod extended_tests {
    use super::*;
    use crate::selector::server_stat_man::ServerStatMan;

    include!("feedback_uri_selector_tests.rs");
}
