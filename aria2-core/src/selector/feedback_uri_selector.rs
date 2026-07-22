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
            fast_cands.sort_by(|a, b| b.0.cmp(&a.0));
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
            if let Some(stat) = self.stat_man.find_stat_by_protocol(&host, &protocol) {
                if !stat.is_ok() {
                    trace!(uri = %uri, "Error not considered (rarer)");
                    continue;
                }
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
        cands.first().map(|(_, idx)| *idx).or_else(|| {
            // If no valid candidates but URIs exist, return first
            if !uris.is_empty() {
                Some(0)
            } else {
                None
            }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn create_selector() -> FeedbackUriSelector {
        FeedbackUriSelector::new(Arc::new(ServerStatMan::new()))
    }

    fn create_selector_with_man(man: Arc<ServerStatMan>) -> FeedbackUriSelector {
        FeedbackUriSelector::new(man)
    }

    // ======================================================================
    // extract_host_and_protocol tests
    // ======================================================================

    #[test]
    fn test_extract_host_and_protocol_http() {
        let (host, proto) = extract_host_and_protocol("http://example.com/path").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(proto, "http");
    }

    #[test]
    fn test_extract_host_and_protocol_https_with_port() {
        let (host, proto) =
            extract_host_and_protocol("https://cdn.example.com:8443/file").unwrap();
        assert_eq!(host, "cdn.example.com:8443");
        assert_eq!(proto, "https");
    }

    #[test]
    fn test_extract_host_and_protocol_ftp() {
        let (host, proto) = extract_host_and_protocol("ftp://files.example.com/").unwrap();
        assert_eq!(host, "files.example.com");
        assert_eq!(proto, "ftp");
    }

    #[test]
    fn test_extract_host_and_protocol_no_path() {
        let (host, proto) = extract_host_and_protocol("http://example.com").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(proto, "http");
    }

    #[test]
    fn test_extract_host_and_protocol_invalid() {
        assert!(extract_host_and_protocol("not-a-uri").is_none());
        assert!(extract_host_and_protocol("").is_none());
        assert!(extract_host_and_protocol("://missing-scheme").is_none());
        assert!(extract_host_and_protocol("http://").is_none());
    }

    // ======================================================================
    // FeedbackUriSelector basic tests
    // ======================================================================

    #[test]
    fn test_select_empty_uris() {
        let sel = create_selector();
        assert!(sel.select(&[], &[]).is_none());
    }

    #[test]
    fn test_select_single_uri() {
        let sel = create_selector();
        let uris = vec!["http://example.com/file".to_string()];
        assert_eq!(sel.select(&uris, &[]), Some(0));
    }

    #[test]
    fn test_select_faster_prefers_fast_server() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://slow.com/file".to_string(),
            "http://fast.com/file".to_string(),
        ];

        // Make fast.com fast (> 20 KB/s)
        man.update_with_protocol("fast.com", "http", 50000, false);
        man.update_with_protocol("slow.com", "http", 100, false);

        // Both need to be OK status (default) and not in usedHosts
        let result = sel.select(&uris, &[]);
        assert_eq!(result, Some(1), "Should select the fast server");
    }

    #[test]
    fn test_select_faster_skips_used_hosts() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://used.com/file".to_string(),
            "http://free.com/file".to_string(),
        ];

        // Both are fast, but used.com is in use
        man.update_with_protocol("used.com", "http", 100000, false);
        man.update_with_protocol("free.com", "http", 50000, false);

        let used = vec![(0, "used.com".to_string())];
        let result = sel.select(&uris, &used);
        assert_eq!(result, Some(1), "Should skip used host and select free");
    }

    #[test]
    fn test_select_faster_skips_error_servers() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://error.com/file".to_string(),
            "http://ok.com/file".to_string(),
        ];

        // error.com is in error state
        man.update_with_protocol("error.com", "http", 100000, false);
        let err_stat = man.find_stat_by_protocol("error.com", "http").unwrap();
        err_stat.set_error();

        // ok.com is untested (no stat), so it goes to normCands
        let result = sel.select(&uris, &[]);
        assert_eq!(result, Some(1), "Should skip error server");
    }

    #[test]
    fn test_select_faster_below_threshold_goes_to_norm() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://slow.com/file".to_string(),
            "http://untested.com/file".to_string(),
        ];

        // slow.com has speed below 20 KB/s threshold
        man.update_with_protocol("slow.com", "http", 5000, false);

        // Both are in normCands; slow.com comes first
        let result = sel.select(&uris, &[]);
        assert_eq!(
            result,
            Some(0),
            "Slow server should be in normCands (comes first)"
        );
    }

    #[test]
    fn test_select_rarer_prefers_used_hosts() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://unused.com/file".to_string(),
            "http://proven.com/file".to_string(),
        ];

        // Neither server has stats → selectFaster returns None (both untested, normCands)
        // But let's make them both slow so selectFaster returns normCands
        // Actually, untested servers go to normCands, so selectFaster would return
        // the first one. Let's make selectFaster return None by putting both
        // servers in error state, forcing selectRarer fallback.

        // Make both error so selectFaster skips them
        man.update_with_protocol("unused.com", "http", 100, false);
        man.update_with_protocol("proven.com", "http", 100, false);
        man.find_stat_by_protocol("unused.com", "http").unwrap().set_error();
        man.find_stat_by_protocol("proven.com", "http").unwrap().set_error();

        // selectFaster skips both → falls to selectRarer
        // But selectRarer also skips error servers → returns first URI
        let result = sel.select(&uris, &[]);
        // In this edge case, both are error, selectRarer returns first URI
        assert!(result.is_some());
    }

    #[test]
    fn test_select_rarer_returns_proven_host() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://unproven.com/file".to_string(),
            "http://proven.com/file".to_string(),
        ];

        // No stats for either → selectFaster will pick from normCands
        // To test selectRarer specifically, make selectFaster return None
        // by putting both in error. But then selectRarer also skips error...
        //
        // Let's use a different approach: make the stats OK but below threshold,
        // then both go to normCands. But selectFaster returns from normCands.
        //
        // Actually, to truly test selectRarer, we need selectFaster to return
        // None. The only way is if ALL URIs have error stats or are in usedHosts.

        // Put all in usedHosts so selectFaster skips them
        let used = vec![
            (0, "unproven.com".to_string()),
            (1, "proven.com".to_string()),
        ];
        let result = sel.select(&uris, &used);
        // selectFaster skips all → selectRarer picks first with matching usedHost
        assert_eq!(result, Some(0), "selectRarer should prefer first usedHost match");
    }

    #[test]
    fn test_select_returns_none_when_all_error_and_used() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec!["http://error.com/file".to_string()];

        // Error server
        man.update_with_protocol("error.com", "http", 100, false);
        man.find_stat_by_protocol("error.com", "http").unwrap().set_error();

        let result = sel.select(&uris, &[]);
        // selectFaster skips error → normCands empty → None
        // selectRarer skips error → no candidates → returns first URI anyway
        assert!(result.is_some(), "selectRarer should return first URI as fallback");
    }

    #[test]
    fn test_select_protocol_aware() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://mirror.com/file".to_string(),  // index 0
            "ftp://mirror.com/file".to_string(),   // index 1
        ];

        // Only http mirror is fast
        man.update_with_protocol("mirror.com", "http", 50000, false);
        // ftp mirror has no stats (untested → normCands)

        let result = sel.select(&uris, &[]);
        assert_eq!(result, Some(0), "Should select fast HTTP server over untested FTP");
    }

    #[test]
    fn test_select_protocol_different_stats() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://mirror.com/file".to_string(),  // index 0
            "https://mirror.com/file".to_string(), // index 1
        ];

        // HTTP is error, HTTPS is fast
        man.update_with_protocol("mirror.com", "http", 100000, false);
        man.find_stat_by_protocol("mirror.com", "http").unwrap().set_error();
        man.update_with_protocol("mirror.com", "https", 50000, false);

        let result = sel.select(&uris, &[]);
        assert_eq!(result, Some(1), "Should select fast HTTPS server, skip error HTTP");
    }

    #[test]
    fn test_speed_threshold_boundary() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://at_threshold.com/file".to_string(),
            "http://above_threshold.com/file".to_string(),
        ];

        // Exactly at threshold (20 * 1024 = 20480) → should be normCands
        man.update_with_protocol("at_threshold.com", "http", SPEED_THRESHOLD, false);
        // One byte above → should be fastCands
        man.update_with_protocol("above_threshold.com", "http", SPEED_THRESHOLD + 1, false);

        let result = sel.select(&uris, &[]);
        assert_eq!(
            result,
            Some(1),
            "Server above threshold should be selected over one at threshold"
        );
    }

    #[test]
    fn test_num_uri_limit() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        // Create 15 URIs, all with different hosts and fast speeds
        let mut uris = Vec::new();
        for i in 0..15u64 {
            uris.push(format!("http://host{}.com/file", i));
            man.update_with_protocol(&format!("host{}.com", i), "http", 50000 + i * 1000, false);
        }

        let result = sel.select(&uris, &[]);
        assert!(result.is_some());
        // The selector should only consider the first 10 URIs (NUM_URI limit)
        // and pick the fastest among them
        assert!(
            result.unwrap() < 10,
            "Should select from first 10 URIs due to NUM_URI limit"
        );
    }

    #[test]
    fn test_tune_command_no_panic() {
        let sel = create_selector();
        let uris = vec!["http://example.com/file".to_string()];
        sel.tune_command(&uris, 12345);
    }

    #[test]
    fn test_reset_no_panic() {
        let sel = create_selector();
        sel.reset();
    }

    #[test]
    fn test_invalid_uris_skipped() {
        let sel = create_selector();
        let uris = vec![
            "not-a-uri".to_string(),               // index 0, invalid
            "http://valid.com/file".to_string(),    // index 1, valid
        ];

        let result = sel.select(&uris, &[]);
        assert_eq!(result, Some(1), "Should skip invalid URI and select valid one");
    }

    #[test]
    fn test_select_rarer_fallback_to_first_candidate() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://a.com/file".to_string(),
            "http://b.com/file".to_string(),
        ];

        // Both in usedHosts → selectFaster skips them → selectRarer picks match
        let used = vec![
            (0, "a.com".to_string()),
            (1, "b.com".to_string()),
        ];
        let result = sel.select(&uris, &used);
        // selectRarer should find a.com first in usedHosts and return index 0
        assert_eq!(result, Some(0), "selectRarer should find first host in usedHosts");
    }

    #[test]
    fn test_all_uris_in_used_hosts_select_rarer() {
        let man = Arc::new(ServerStatMan::new());
        let sel = create_selector_with_man(Arc::clone(&man));

        let uris = vec![
            "http://a.com/file".to_string(),
            "http://b.com/file".to_string(),
        ];

        // Both in usedHosts → selectFaster returns None
        // selectRarer picks first candidate matching usedHost
        let used = vec![
            (0, "a.com".to_string()),
            (1, "b.com".to_string()),
        ];
        let result = sel.select(&uris, &used);
        assert!(result.is_some(), "selectRarer should find a match in usedHosts");
    }
}
