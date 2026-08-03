//! Adaptive URI selector matching C++ aria2's `AdaptiveURISelector`.
//!
//! This selector returns one of the best mirrors for first and reserved
//! connections. For supplementary ones, it returns mirrors that have not
//! been tested yet, and if all have been tested, returns mirrors that
//! need to be tested again. Otherwise, it does not return more mirrors.
//!
//! # Algorithm (from C++)
//!
//! 1. At least 3 mirrors must be tested before choosing the best.
//! 2. For supplementary connections (nbServerToEvaluate > 0):
//!    - Prefer untested mirrors
//!    - Then mirrors that haven't been tested recently (exponential backoff)
//!    - Then the best mirror by speed
//! 3. `getBestMirror()` selects from a 25% speed range with random tie-breaking.
//! 4. `adjustLowestSpeedLimit()` lowers the speed limit when max speed is unknown.
//!
//! # C++ Reference
//!
//! Based on `AdaptiveURISelector.h/.cc` from both aria2_original and aria2-next.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use tracing::trace;

use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

// ---------------------------------------------------------------------------
// Constants (matching C++ aria2)
// ---------------------------------------------------------------------------

/// Maximum timeout for retry with increased timeout (60 seconds).
/// From C++: `constexpr auto MAX_TIMEOUT = 60_s;`
const MAX_TIMEOUT_SECS: u64 = 60;

/// Floor value for lowest speed limit when max speed is unknown.
/// From C++: `int low_lowest = 4_k;` (4096 bytes/sec)
const LOW_LOWEST_SPEED_LIMIT: u64 = 4096;

/// Minimum number of tested servers before selecting best mirror.
/// From C++: `if (getNbTestedServers(uris) < 3)`
const MIN_TESTED_SERVERS: usize = 3;

/// Maximum counter value for exponential backoff retest.
/// From C++: `if (counter > 8) continue;`
const MAX_RETEST_COUNTER: u32 = 8;

/// Speed range percentage for best mirror selection.
/// From C++: `int min = max - (int)(max * 0.25);`
const BEST_MIRROR_RANGE_PCT: f64 = 0.25;

// ---------------------------------------------------------------------------
// URI parsing helpers
// ---------------------------------------------------------------------------

fn extract_host(uri: &str) -> Option<String> {
    crate::selector::feedback_uri_selector::extract_host_and_protocol(uri).map(|(h, _)| h)
}

fn extract_host_and_protocol(uri: &str) -> Option<(String, String)> {
    crate::selector::feedback_uri_selector::extract_host_and_protocol(uri)
}

/// Extract (index, host, protocol) triples from URI list.
fn extract_hosts(uris: &[String]) -> Vec<(usize, String, String)> {
    uris.iter()
        .enumerate()
        .filter_map(|(i, u)| extract_host_and_protocol(u).map(|(h, p)| (i, h, p)))
        .collect()
}

/// Get ServerStat for a URI via (host, protocol) lookup.
#[allow(dead_code)]
fn get_server_stats(
    stat_man: &ServerStatMan,
    uri: &str,
) -> Option<Arc<crate::selector::server_stat::ServerStat>> {
    let (host, protocol) = extract_host_and_protocol(uri)?;
    stat_man.find_stat_by_protocol(&host, &protocol)
}

/// Get the max speed for a URI: max(single_avg, multi_avg).
/// Matches C++ `getUriMaxSpeed()`.
#[allow(dead_code)]
fn get_uri_max_speed(stat_man: &ServerStatMan, uri: &str) -> u64 {
    get_server_stats(stat_man, uri)
        .map(|s| s.get_single_avg_speed().max(s.get_multi_avg_speed()))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// AdaptiveUriSelector
// ---------------------------------------------------------------------------

pub struct AdaptiveUriSelector {
    stat_man: Arc<ServerStatMan>,
    uris: Vec<String>,
    nb_server_to_evaluate: AtomicI32,
    nb_connections: AtomicI32,
    /// Current timeout for retry-with-increased-timeout logic.
    timeout_secs: AtomicUsize,
}

impl AdaptiveUriSelector {
    /// Create a new selector with default counters.
    pub fn new(stat_man: Arc<ServerStatMan>) -> Self {
        Self {
            stat_man,
            uris: Vec::new(),
            nb_server_to_evaluate: AtomicI32::new(
                crate::constants::DEFAULT_NB_SERVER_TO_EVALUATE as i32,
            ),
            nb_connections: AtomicI32::new(crate::constants::DEFAULT_NB_CONNECTIONS as i32),
            timeout_secs: AtomicUsize::new(0),
        }
    }

    /// Create with a known URI list (for report_success/report_failure).
    pub fn new_with_uris(stat_man: Arc<ServerStatMan>, uris: Vec<String>) -> Self {
        Self {
            stat_man,
            uris,
            nb_server_to_evaluate: AtomicI32::new(
                crate::constants::DEFAULT_NB_SERVER_TO_EVALUATE as i32,
            ),
            nb_connections: AtomicI32::new(crate::constants::DEFAULT_NB_CONNECTIONS as i32),
            timeout_secs: AtomicUsize::new(0),
        }
    }

    pub fn set_uris(&mut self, uris: Vec<String>) {
        self.uris = uris;
    }

    pub fn get_uris(&self) -> &[String] {
        &self.uris
    }

    pub fn set_nb_connections(&self, n: i32) {
        self.nb_connections.store(n, Ordering::Relaxed);
    }

    pub fn set_nb_evaluate(&self, n: i32) {
        self.nb_server_to_evaluate.store(n, Ordering::Relaxed);
    }

    /// Set the initial timeout (from RequestGroup option).
    pub fn set_timeout_secs(&self, secs: usize) {
        self.timeout_secs.store(secs, Ordering::Relaxed);
    }

    pub fn stat_man(&self) -> &Arc<ServerStatMan> {
        &self.stat_man
    }

    // ====================================================================
    // Core selection algorithm (matching C++ AdaptiveURISelector::selectOne)
    // ====================================================================

    /// Select one URI using the adaptive algorithm.
    ///
    /// Matches C++ `AdaptiveURISelector::selectOne()`.
    fn select_one(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize> {
        if uris.is_empty() {
            return None;
        }

        let nb_conn = self.nb_connections.fetch_add(1, Ordering::Relaxed);

        // Single URI shortcut
        if uris.len() == 1 {
            return Some(0);
        }

        let hosts = extract_hosts(uris);
        if hosts.is_empty() {
            return Some(0);
        }

        // At least MIN_TESTED_SERVERS (3) mirrors must be tested
        let nb_tested = self.get_nb_tested_servers(&hosts);
        if nb_tested < MIN_TESTED_SERVERS
            && let Some(idx) = self.get_first_not_tested(&hosts)
        {
            trace!(idx, "AdaptiveURISelector: choosing first non-tested mirror");
            self.nb_server_to_evaluate.fetch_sub(1, Ordering::Relaxed);
            return Some(idx);
        }

        // Check if we should evaluate servers or select best
        let nb_eval = self.nb_server_to_evaluate.load(Ordering::Relaxed);
        if nb_eval > 0 && nb_conn > 1 {
            self.nb_server_to_evaluate.fetch_sub(1, Ordering::Relaxed);

            // Prefer untested mirror
            if let Some(idx) = self.get_first_not_tested(&hosts) {
                trace!(
                    idx,
                    nb_conn, "AdaptiveURISelector: choosing non-tested mirror"
                );
                return Some(idx);
            }

            // Then mirror that hasn't been tested recently (exponential backoff)
            if let Some(idx) = self.get_first_to_test_uri(&hosts) {
                trace!(idx, nb_conn, "AdaptiveURISelector: choosing re-test mirror");
                return Some(idx);
            }

            // Fall back to best mirror
            return self.get_best_mirror(&hosts, used_hosts);
        }

        // Select best mirror
        self.get_best_mirror(&hosts, used_hosts)
    }

    // ====================================================================
    // Helper methods (matching C++ AdaptiveURISelector private methods)
    // ====================================================================

    /// Find the first URI with no ServerStat entry (untested mirror).
    /// Matches C++ `getFirstNotTestedUri()`.
    fn get_first_not_tested(&self, hosts: &[(usize, String, String)]) -> Option<usize> {
        for (idx, host, protocol) in hosts {
            if self
                .stat_man
                .find_stat_by_protocol(host, protocol)
                .is_none()
            {
                return Some(*idx);
            }
        }
        None
    }

    /// Find the first URI that should be retested.
    ///
    /// Matches C++ `getFirstToTestUri()`. Uses exponential backoff:
    /// retest if not tested since `2^counter` days. Counter capped at 8.
    fn get_first_to_test_uri(&self, hosts: &[(usize, String, String)]) -> Option<usize> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for (idx, host, protocol) in hosts {
            let stat = match self.stat_man.find_stat_by_protocol(host, protocol) {
                Some(s) => s,
                None => continue,
            };

            let counter = stat.get_counter();
            if counter > MAX_RETEST_COUNTER {
                continue;
            }

            // Retest if not tested since 2^counter days
            let retest_hours = 1u64.checked_shl(counter).unwrap_or(u64::MAX) * 24;
            let last_updated = stat.get_last_updated();
            if last_updated > 0 && now_secs.saturating_sub(last_updated) > retest_hours * 3600 {
                return Some(*idx);
            }
        }
        None
    }

    /// Count the number of tested servers (those with a ServerStat entry).
    /// Matches C++ `getNbTestedServers()`.
    fn get_nb_tested_servers(&self, hosts: &[(usize, String, String)]) -> usize {
        hosts
            .iter()
            .filter(|(_, host, protocol)| {
                self.stat_man
                    .find_stat_by_protocol(host, protocol)
                    .is_some()
            })
            .count()
    }

    /// Select the best mirror, using a 25% speed range with random tie-breaking.
    ///
    /// Matches C++ `getBestMirror()`.
    fn get_best_mirror(
        &self,
        hosts: &[(usize, String, String)],
        used_hosts: &[(usize, String)],
    ) -> Option<usize> {
        let max_speed = self.get_max_download_speed(hosts);
        let min_speed = (max_speed as f64 * (1.0 - BEST_MIRROR_RANGE_PCT)) as u64;
        let mut bests = self.get_uris_by_speed(hosts, min_speed);

        // Filter out used hosts
        let used_set: std::collections::HashSet<&str> =
            used_hosts.iter().map(|(_, h)| h.as_str()).collect();
        bests.retain(|idx| {
            hosts
                .get(*idx)
                .is_some_and(|(_, h, _)| !used_set.contains(h.as_str()))
        });

        if bests.is_empty() {
            // All hosts are used — fall back to any host with speed > 0
            bests = self.get_uris_by_speed(hosts, 0);
            bests.retain(|idx| {
                hosts
                    .get(*idx)
                    .is_some_and(|(_, h, _)| !used_set.contains(h.as_str()))
            });
        }

        // If still empty after filtering (all hosts used), accept any host
        if bests.is_empty() {
            bests = self.get_uris_by_speed(hosts, 0);
        }

        if bests.len() == 1 {
            Some(bests[0])
        } else if bests.len() < 2 {
            self.get_max_download_speed_uri(hosts)
        } else {
            let idx = self.select_random_uri(&bests);
            Some(idx)
        }
    }

    /// Get the maximum download speed across all URIs.
    fn get_max_download_speed(&self, hosts: &[(usize, String, String)]) -> u64 {
        hosts.iter().fold(0u64, |max, (_, host, protocol)| {
            if let Some(stat) = self.stat_man.find_stat_by_protocol(host, protocol) {
                let speed = stat.get_single_avg_speed().max(stat.get_multi_avg_speed());
                max.max(speed)
            } else {
                max
            }
        })
    }

    /// Get the URI index with the maximum download speed.
    fn get_max_download_speed_uri(&self, hosts: &[(usize, String, String)]) -> Option<usize> {
        let mut max_speed: i64 = -1;
        let mut best_idx: Option<usize> = None;

        for (idx, host, protocol) in hosts {
            if let Some(stat) = self.stat_man.find_stat_by_protocol(host, protocol) {
                let single = stat.get_single_avg_speed() as i64;
                let multi = stat.get_multi_avg_speed() as i64;
                if single > max_speed {
                    max_speed = single;
                    best_idx = Some(*idx);
                }
                if multi > max_speed {
                    max_speed = multi;
                    best_idx = Some(*idx);
                }
            }
        }
        best_idx
    }

    /// Get URIs with speed above the given minimum.
    fn get_uris_by_speed(&self, hosts: &[(usize, String, String)], min: u64) -> Vec<usize> {
        hosts
            .iter()
            .filter_map(|(idx, host, protocol)| {
                let stat = self.stat_man.find_stat_by_protocol(host, protocol)?;
                if stat.get_single_avg_speed() > min || stat.get_multi_avg_speed() > min {
                    Some(*idx)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Randomly select a URI index from a list.
    fn select_random_uri(&self, indices: &[usize]) -> usize {
        let mut rng = rand::thread_rng();
        let pos = rng.gen_range(0..indices.len());
        indices[pos]
    }

    /// Adjust lowest speed limit based on known max download speed.
    ///
    /// Returns the adjusted limit or 0 if no adjustment is needed.
    pub fn adjust_lowest_speed_limit(&self, uris: &[String], lowest_limit: u64) -> u64 {
        if lowest_limit == 0 {
            return 0;
        }

        let hosts = extract_hosts(uris);
        let max = self.get_max_download_speed(&hosts);

        if max > 0 && lowest_limit > max / 4 {
            max / 4
        } else if max == 0 && lowest_limit > LOW_LOWEST_SPEED_LIMIT {
            LOW_LOWEST_SPEED_LIMIT
        } else {
            0
        }
    }

    /// Try to retry failed URIs with increased timeout.
    ///
    /// Returns timeouted URIs that should be added back, if any.
    pub fn may_retry_with_increased_timeout(&self) -> Option<Vec<String>> {
        let current = self.timeout_secs.load(Ordering::Relaxed) as u64;
        let new_timeout = current * 2;
        if new_timeout >= MAX_TIMEOUT_SECS {
            return None;
        }
        self.timeout_secs
            .store(new_timeout as usize, Ordering::Relaxed);
        Some(Vec::new())
    }

    pub fn reset_counters(&self) {
        self.nb_connections.store(1, Ordering::Relaxed);
        for stat in self.stat_man.get_all_stats() {
            stat.reset_counter();
        }
    }

    // ====================================================================
    // Report success/failure
    // ====================================================================

    /// Report a successful download from a specific URI.
    pub fn report_success(&self, uri_idx: usize, speed: u64, is_multi: bool) {
        if let Some(uri) = self.uris.get(uri_idx)
            && let Some(host) = extract_host(uri)
        {
            self.stat_man.update(&host, speed, is_multi);
            if let Some(stat) = self.stat_man.find_stat(&host) {
                stat.reset_status();
            }
        }
    }

    /// Report a failed download with error code.
    pub fn report_failure_with_code(&self, uri_idx: usize, error_code: u16) {
        if let Some(uri) = self.uris.get(uri_idx)
            && let Some(host) = extract_host(uri)
        {
            self.stat_man.get_or_create(&host);
            self.stat_man.mark_failure(&host, error_code);
        }
    }

    /// Report failure with default error code (500).
    pub fn report_failure_default(&self, uri_idx: usize) {
        self.report_failure_with_code(uri_idx, 500);
    }
}

impl UriSelector for AdaptiveUriSelector {
    fn select(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize> {
        self.select_one(uris, used_hosts)
    }

    fn tune_command(&self, uris: &[String], _speed: u64) {
        let limit = self.adjust_lowest_speed_limit(uris, 0);
        if limit > 0 {
            tracing::debug!("AdaptiveURISelector tuning lowest-speed-limit to {}", limit);
        }
    }

    fn reset(&self) {
        self.reset_counters();
    }

    fn report_failure(&mut self, uri_idx: usize) {
        self.report_failure_with_code(uri_idx, 500);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::server_stat_man::ServerStatMan;

    include!("adaptive_uri_selector_tests.rs");
}
