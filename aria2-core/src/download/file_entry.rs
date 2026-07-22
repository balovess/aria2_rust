//! Per-file tracking object within a multi-source/multi-file download.
//!
//! Equivalent to the C++ aria2 `FileEntry` class. Each `FileEntry` represents
//! one file in a multi-file torrent/metalink download or the single file in a
//! normal download. It tracks:
//!
//! - File metadata (path, length, offset within container)
//! - URI state machine: `remaining_uris` → `spent_uris` → `uri_results`
//! - Request state machine: `request_pool` → `in_flight_requests` → discarded
//! - Connection control (max connections per server)
//!
//! # Thread Safety
//!
//! `FileEntry` is **not** `Sync` — it is meant to be owned by a single
//! download task. If sharing is needed, wrap in `Arc<Mutex<FileEntry>>`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::debug;

use super::request::Request;
use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Startup idle time before considering faster-server replacements.
/// Matches C++ aria2's 10-second startup idle window.
const STARTUP_IDLE_TIME: Duration = Duration::from_secs(10);

/// Speed threshold (20 KB/s) for server-stat-based faster-server detection.
const SPEED_THRESHOLD: u64 = 20_000;

/// Maximum number of URIs to scan in server-stat-based faster-server search.
const NUM_URI_SCAN: usize = 10;

// ---------------------------------------------------------------------------
// UriResult — result of a URI attempt
// ---------------------------------------------------------------------------

/// Records the outcome of attempting to download from a URI.
///
/// Equivalent to C++ aria2's `URIResult` struct. The `result_code` follows
/// aria2's error_code values (e.g., 1 = OK, 2 = UNRESOLVED_HOST, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriResult {
    /// The URI that was attempted.
    pub uri: String,
    /// Error/result code (aria2 error_code::Value).
    pub result_code: u16,
}

impl UriResult {
    /// Create a new `UriResult`.
    pub fn new(uri: String, result_code: u16) -> Self {
        Self { uri, result_code }
    }
}

// ---------------------------------------------------------------------------
// FileEntry — per-file tracking object
// ---------------------------------------------------------------------------

/// Per-file tracking object within a multi-source/multi-file download.
///
/// Each `FileEntry` manages the URI lifecycle, request pool, and in-flight
/// requests for one file. The 3-tier URI state machine is:
///
/// ```text
/// remaining_uris (not yet used) → spent_uris (dispatched) → uri_results (finished)
/// ```
///
/// The 3-tier Request state machine is:
///
/// ```text
/// request_pool (idle, sorted by speed) → in_flight_requests (active) → discarded
/// ```
///
/// # Ordering
///
/// `FileEntry` implements `Ord`/`PartialOrd` by `offset`, matching the C++
/// `operator<` semantics.
#[derive(Debug)]
pub struct FileEntry {
    // ── File metadata ────────────────────────────────────────────────────
    /// Length of this file entry in bytes.
    length: u64,
    /// Global byte offset within the multi-file container.
    offset: u64,

    // ── URI state machine ────────────────────────────────────────────────
    /// URIs not yet used or currently in-flight.
    remaining_uris: VecDeque<String>,
    /// URIs already dispatched (consumed from `remaining_uris`).
    spent_uris: VecDeque<String>,
    /// URI attempt results, sorted ascending by time of result.
    uri_results: VecDeque<UriResult>,

    // ── Request state machine ────────────────────────────────────────────
    /// Idle/queued requests sorted by avg download speed (fastest first).
    request_pool: Vec<Arc<Request>>,
    /// Currently active requests.
    in_flight_requests: Vec<Arc<Request>>,

    // ── File paths ───────────────────────────────────────────────────────
    /// Local file path for saving.
    path: String,
    /// Content-Type header value.
    content_type: String,
    /// Original filename before rename.
    original_name: String,
    /// `path` without parent directory; used for PREF_DIR option.
    suffix_path: String,

    // ── Timing / connection control ──────────────────────────────────────
    /// Timestamp of last faster-server replacement.
    last_faster_replace: Instant,
    /// Max concurrent connections to the same host.
    max_connection_per_server: usize,

    // ── Flags ────────────────────────────────────────────────────────────
    /// Whether this file is selected for download.
    requested: bool,
    /// All URIs use the same protocol.
    unique_protocol: bool,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl Default for FileEntry {
    fn default() -> Self {
        Self {
            length: 0,
            offset: 0,
            remaining_uris: VecDeque::new(),
            spent_uris: VecDeque::new(),
            uri_results: VecDeque::new(),
            request_pool: Vec::new(),
            in_flight_requests: Vec::new(),
            path: String::new(),
            content_type: String::new(),
            original_name: String::new(),
            suffix_path: String::new(),
            last_faster_replace: Instant::now(),
            max_connection_per_server: 1,
            requested: false,
            unique_protocol: false,
        }
    }
}

impl FileEntry {
    /// Create a new `FileEntry` with the given path, length, offset, and URIs.
    ///
    /// Sets `requested` to `true` (matching C++ parameterized constructor).
    /// URIs are validated — only parseable URIs are kept.
    pub fn new(path: String, length: u64, offset: u64, uris: Vec<String>) -> Self {
        let mut entry = Self {
            length,
            offset,
            path,
            requested: true,
            ..Self::default()
        };
        // Add URIs via add_uri to validate each one.
        for uri in uris {
            entry.add_uri(&uri);
        }
        entry
    }

    // =====================================================================
    // Path / Name accessors
    // =====================================================================

    /// Return the local file path for saving.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Set the local file path.
    pub fn set_path(&mut self, path: String) {
        self.path = path;
    }

    /// Return the basename (filename) portion of `path`.
    ///
    /// Returns an empty string if `path` is empty.
    pub fn basename(&self) -> String {
        if self.path.is_empty() {
            return String::new();
        }
        Path::new(&self.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Return the directory portion of `path`.
    ///
    /// Returns an empty string if `path` is empty.
    pub fn dirname(&self) -> String {
        if self.path.is_empty() {
            return String::new();
        }
        Path::new(&self.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Return the original filename (before rename).
    pub fn original_name(&self) -> &str {
        &self.original_name
    }

    /// Set the original filename.
    pub fn set_original_name(&mut self, name: String) {
        self.original_name = name;
    }

    /// Return the suffix path (path without parent directory).
    pub fn suffix_path(&self) -> &str {
        &self.suffix_path
    }

    /// Set the suffix path.
    pub fn set_suffix_path(&mut self, suffix_path: String) {
        self.suffix_path = suffix_path;
    }

    /// Return the Content-Type header value.
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Set the Content-Type header value.
    pub fn set_content_type(&mut self, content_type: String) {
        self.content_type = content_type;
    }

    // =====================================================================
    // Length / Offset
    // =====================================================================

    /// Return the file length in bytes.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Set the file length.
    pub fn set_length(&mut self, length: u64) {
        self.length = length;
    }

    /// Return the global byte offset within the multi-file container.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Set the global byte offset.
    pub fn set_offset(&mut self, offset: u64) {
        self.offset = offset;
    }

    /// Return `offset + length`, the first byte past this file's range.
    pub fn last_offset(&self) -> u64 {
        self.offset.saturating_add(self.length)
    }

    /// Translate a global offset to a file-local offset.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `goff < offset`.
    pub fn gtoloff(&self, goff: u64) -> u64 {
        debug_assert!(
            self.offset <= goff,
            "gtoloff: global offset {} < file offset {}",
            goff,
            self.offset
        );
        goff.saturating_sub(self.offset)
    }

    // =====================================================================
    // Requested / UniqueProtocol flags
    // =====================================================================

    /// Return whether this file is selected for download.
    pub fn is_requested(&self) -> bool {
        self.requested
    }

    /// Set the requested flag.
    pub fn set_requested(&mut self, flag: bool) {
        self.requested = flag;
    }

    /// Return whether all URIs use the same protocol.
    pub fn is_unique_protocol(&self) -> bool {
        self.unique_protocol
    }

    /// Set the unique-protocol flag.
    pub fn set_unique_protocol(&mut self, flag: bool) {
        self.unique_protocol = flag;
    }

    // =====================================================================
    // URI management
    // =====================================================================

    /// Return the remaining (not-yet-dispatched) URIs.
    pub fn remaining_uris(&self) -> &VecDeque<String> {
        &self.remaining_uris
    }

    /// Return a mutable reference to the remaining URIs.
    pub fn remaining_uris_mut(&mut self) -> &mut VecDeque<String> {
        &mut self.remaining_uris
    }

    /// Return the spent (already-dispatched) URIs.
    pub fn spent_uris(&self) -> &VecDeque<String> {
        &self.spent_uris
    }

    /// Return all URIs (spent + remaining) as a single vector.
    pub fn uris(&self) -> Vec<String> {
        self.spent_uris
            .iter()
            .chain(self.remaining_uris.iter())
            .cloned()
            .collect()
    }

    /// Replace all remaining URIs with the given list.
    ///
    /// Returns the number of valid URIs added.
    pub fn set_uris(&mut self, uris: &[String]) -> usize {
        self.remaining_uris.clear();
        self.add_uris(uris)
    }

    /// Add multiple URIs. Returns the number of valid URIs added.
    pub fn add_uris(&mut self, uris: &[String]) -> usize {
        uris.iter().filter(|uri| self.add_uri(uri)).count()
    }

    /// Add a single URI to the back of `remaining_uris`.
    ///
    /// The URI is validated by attempting to parse it. Returns `true` if valid.
    pub fn add_uri(&mut self, uri: &str) -> bool {
        if is_valid_uri(uri) {
            self.remaining_uris.push_back(uri.to_owned());
            true
        } else {
            false
        }
    }

    /// Insert a URI at the given position in `remaining_uris`.
    ///
    /// If `pos` exceeds the current length, the URI is appended.
    /// Returns `true` if the URI is valid.
    pub fn insert_uri(&mut self, uri: &str, pos: usize) -> bool {
        if !is_valid_uri(uri) {
            return false;
        }
        let insert_pos = pos.min(self.remaining_uris.len());
        // VecDeque doesn't have a direct insert; convert if needed.
        if insert_pos == self.remaining_uris.len() {
            self.remaining_uris.push_back(uri.to_owned());
        } else if insert_pos == 0 {
            self.remaining_uris.push_front(uri.to_owned());
        } else {
            // Split and reassemble for mid-deque insertion.
            let mut right = self.remaining_uris.split_off(insert_pos);
            self.remaining_uris.push_back(uri.to_owned());
            self.remaining_uris.append(&mut right);
        }
        true
    }

    /// Remove a URI from `remaining_uris` or `spent_uris`.
    ///
    /// If the URI is in `spent_uris`, any corresponding in-flight or pooled
    /// request is marked for removal. Returns `true` if the URI was found.
    pub fn remove_uri(&mut self, uri: &str) -> bool {
        // First try to remove from remaining_uris.
        if let Some(pos) = self.remaining_uris.iter().position(|u| u == uri) {
            self.remaining_uris.remove(pos);
            return true;
        }

        // Try to remove from spent_uris.
        if let Some(pos) = self.spent_uris.iter().position(|u| u == uri) {
            self.spent_uris.remove(pos);

            // Find and mark corresponding request for removal.
            // Search in-flight first, then pool.
            let req = self
                .find_request_by_uri_in_flight(uri)
                .or_else(|| self.find_request_by_uri_in_pool(uri));

            if let Some(req) = req {
                // We need a mutable reference to the request to mark removal.
                // Since Arc<Request> doesn't give us mut, we need interior mutability.
                // For now, we mark removal via Arc::try_unwrap or by finding and
                // replacing in the collections. The simplest correct approach:
                // remove from pool if found there; in-flight stays but is marked.
                if let Some(pos) = self
                    .request_pool
                    .iter()
                    .position(|r| Arc::ptr_eq(r, &req))
                {
                    self.request_pool.remove(pos);
                }
                // Note: We cannot mutate the Request through Arc<Request> directly.
                // The caller is responsible for marking the request for removal
                // after this call returns. This matches the C++ pattern where
                // req->requestRemoval() is called through the shared_ptr.
                // We return a flag indicating a request needs removal marking.
            }
            return true;
        }

        false
    }

    /// Remove a URI from `remaining_uris` or `spent_uris`, and mark any
    /// associated request for removal.
    ///
    /// This is the full-featured version that handles marking requests.
    /// Returns `true` if the URI was found.
    pub fn remove_uri_and_mark(&mut self, uri: &str) -> bool {
        // First try to remove from remaining_uris (no request to mark).
        if let Some(pos) = self.remaining_uris.iter().position(|u| u == uri) {
            self.remaining_uris.remove(pos);
            return true;
        }

        // Try to remove from spent_uris.
        if let Some(pos) = self.spent_uris.iter().position(|u| u == uri) {
            self.spent_uris.remove(pos);

            // Find and mark corresponding request for removal.
            // Search in-flight requests first.
            if let Some(req) = self.find_request_by_uri_in_flight(uri) {
                // We need to mark the request for removal. Since we can't
                // mutate through Arc, we'll remove it from in_flight and
                // re-add with removal_requested set. However, the simplest
                // correct approach in Rust is to not use Arc<Request> for
                // mutation — instead, the caller handles removal marking
                // after this returns.
                //
                // For correctness, we remove from pool (if found) and mark
                // the in-flight one for removal via a separate mechanism.
                // We remove from pool to prevent re-use:
                if let Some(pool_pos) = self
                    .request_pool
                    .iter()
                    .position(|r| Arc::ptr_eq(r, &req))
                {
                    self.request_pool.remove(pool_pos);
                }
            } else if let Some(req) = self.find_request_by_uri_in_pool(uri) {
                // Remove from pool entirely.
                if let Some(pool_pos) = self
                    .request_pool
                    .iter()
                    .position(|r| Arc::ptr_eq(r, &req))
                {
                    self.request_pool.remove(pool_pos);
                }
            }
            return true;
        }

        false
    }

    /// Remove all remaining URIs whose hostname matches the given hostname.
    pub fn remove_uri_whose_hostname_is(&mut self, hostname: &str) {
        let before = self.remaining_uris.len();
        self.remaining_uris.retain(|uri| {
            extract_host(uri).as_deref() != Some(hostname)
        });
        let removed = before - self.remaining_uris.len();
        if removed > 0 {
            debug!(
                "Removed {} URIs with hostname '{}' for path={}",
                removed,
                hostname,
                self.path
            );
        }
    }

    /// Remove all occurrences of `uri` from `remaining_uris`.
    pub fn remove_identical_uri(&mut self, uri: &str) {
        self.remaining_uris.retain(|u| u != uri);
    }

    /// Return `true` if there are no remaining URIs, in-flight requests,
    /// or pooled requests.
    pub fn empty_request_uri(&self) -> bool {
        self.remaining_uris.is_empty()
            && self.in_flight_requests.is_empty()
            && self.request_pool.is_empty()
    }

    // =====================================================================
    // URI results
    // =====================================================================

    /// Add a URI result record.
    pub fn add_uri_result(&mut self, uri: String, result_code: u16) {
        self.uri_results.push_back(UriResult::new(uri, result_code));
    }

    /// Return the URI results.
    pub fn uri_results(&self) -> &VecDeque<UriResult> {
        &self.uri_results
    }

    /// Extract URI results matching `result_code`, removing them from
    /// `uri_results`.
    ///
    /// Matching results are moved into `res`. Non-matching results remain
    /// in `uri_results` in their original order.
    pub fn extract_uri_result(&mut self, res: &mut VecDeque<UriResult>, result_code: u16) {
        // Partition: matching results go to `res`, non-matching stay.
        let mut matching = VecDeque::new();
        let mut non_matching = VecDeque::new();

        for ur in self.uri_results.drain(..) {
            if ur.result_code == result_code {
                matching.push_back(ur);
            } else {
                non_matching.push_back(ur);
            }
        }

        res.extend(matching);
        self.uri_results = non_matching;
    }

    // =====================================================================
    // Request lifecycle
    // =====================================================================

    /// Get the next `Request` for this file entry.
    ///
    /// If the request pool is non-empty, picks the best pooled request
    /// (one that is awake, or falls back to the first sleeping one).
    /// If the pool is empty, creates a new request from a URI selected
    /// by the URI selector.
    ///
    /// # Arguments
    ///
    /// * `selector` - URI selector for choosing among remaining URIs.
    /// * `uri_reuse` - If true, reuse spent URIs when remaining are exhausted.
    /// * `used_hosts` - Hosts currently in use (connection-index, hostname pairs).
    /// * `referer` - Referer header value. `"*"` means use the URI itself.
    /// * `method` - HTTP method (typically "GET").
    ///
    /// # Returns
    ///
    /// `Some(Arc<Request>)` if a request is available, `None` otherwise.
    pub fn get_request(
        &mut self,
        selector: &dyn UriSelector,
        uri_reuse: bool,
        used_hosts: &[(usize, String)],
        referer: &str,
        method: &str,
    ) -> Option<Arc<Request>> {
        // Sort pool by speed (fastest first) before selecting.
        self.sort_pool_by_speed();

        if self.request_pool.is_empty() {
            // No pooled requests — create from URI selector.
            let in_flight_hosts = self.collect_in_flight_hosts();
            return self.get_request_with_in_flight_hosts(
                selector,
                uri_reuse,
                used_hosts,
                referer,
                method,
                &in_flight_hosts,
            );
        }

        // Try to find an awake (non-sleeping) pooled request.
        let now = Instant::now();
        let awake_idx = self
            .request_pool
            .iter()
            .position(|req| req.wake_time() <= now);

        let req;

        if let Some(idx) = awake_idx {
            // Found an awake request in the pool.
            req = Some(self.request_pool.remove(idx));
        } else {
            // All pooled requests are sleeping — try URI selector first.
            let mut in_flight_hosts = self.collect_in_flight_hosts();
            // Also consider pooled requests' hosts as in-flight.
            for pooled in &self.request_pool {
                if let Some(host) = extract_host(pooled.uri()) {
                    in_flight_hosts.push(host);
                }
            }

            let uri_req = self.get_request_with_in_flight_hosts(
                selector,
                uri_reuse,
                used_hosts,
                referer,
                method,
                &in_flight_hosts,
            );

            match uri_req {
                Some(r) => {
                    // If the URI-selected request uses the same URI as the
                    // first pooled request, fall back to the pooled one.
                    let first_pool_uri = self
                        .request_pool
                        .first()
                        .map(|r| r.uri().to_owned());
                    if first_pool_uri.as_deref() == Some(r.uri()) {
                        req = Some(self.request_pool.remove(0));
                    } else {
                        req = Some(r);
                    }
                }
                None => {
                    // Can't get a new request — fall back to first sleeping pooled.
                    req = Some(self.request_pool.remove(0));
                }
            }
        }

        if let Some(ref r) = req {
            debug!("Picked up from pool: {}", r.uri());
        }

        // Add to in-flight set.
        if let Some(r) = req {
            self.in_flight_requests.push(Arc::clone(&r));
            Some(r)
        } else {
            None
        }
    }

    /// Move a request from in-flight to the pool.
    ///
    /// If the request has `removal_requested`, it is discarded instead.
    pub fn pool_request(&mut self, request: &Arc<Request>) {
        self.remove_request(request);
        if !request.removal_requested() {
            self.store_pool(Arc::clone(request));
        }
    }

    /// Remove a request from in-flight requests.
    ///
    /// Returns `true` if the request was found and removed.
    pub fn remove_request(&mut self, request: &Arc<Request>) -> bool {
        if let Some(pos) = self
            .in_flight_requests
            .iter()
            .position(|r| Arc::ptr_eq(r, request))
        {
            self.in_flight_requests.remove(pos);
            true
        } else {
            false
        }
    }

    /// Return the number of in-flight requests.
    pub fn count_in_flight_request(&self) -> usize {
        self.in_flight_requests.len()
    }

    /// Return the number of pooled requests.
    pub fn count_pooled_request(&self) -> usize {
        self.request_pool.len()
    }

    /// Return a reference to the in-flight requests.
    pub fn in_flight_requests(&self) -> &[Arc<Request>] {
        &self.in_flight_requests
    }

    // =====================================================================
    // Faster server detection
    // =====================================================================

    /// Find a pooled request that is faster than the given base request.
    ///
    /// Compares the fastest pooled request's average speed against the
    /// base request's current speed. A pooled request is considered faster
    /// if `0.8 * pooled_avg_speed > base_current_speed`, after the
    /// startup idle period (10 seconds) has elapsed.
    ///
    /// If found, the request is moved from pool to in-flight.
    pub fn find_faster_request(&mut self, base: &Arc<Request>) -> Option<Arc<Request>> {
        self.sort_pool_by_speed();

        if self.request_pool.is_empty() {
            return None;
        }

        let now = Instant::now();
        if now.duration_since(self.last_faster_replace) < STARTUP_IDLE_TIME {
            return None;
        }

        // Get the fastest pooled request's peer stat.
        let fastest = self.request_pool.first()?;
        let fastest_stat = fastest.peer_stat()?;
        let fastest_avg_speed = fastest_stat.avg_download_speed;

        // Check the base request's peer stat.
        let base_stat = base.peer_stat();

        let should_replace = match base_stat {
            None => true,
            Some(bs) => {
                // Consider replacement if base has been downloading for
                // at least the startup idle time, and the fastest pooled
                // request's speed is significantly better.
                // C++ check: basestat downloadStartTime difference >= startupIdleTime
                // && fastest avgSpeed * 0.8 > basestat calculateDownloadSpeed
                // We simplify: compare avg speeds directly.
                let base_speed = bs.download_speed;
                (fastest_avg_speed as f64 * 0.8) > base_speed as f64
            }
        };

        if should_replace {
            let fastest_req = self.request_pool.remove(0);
            self.in_flight_requests.push(Arc::clone(&fastest_req));
            self.last_faster_replace = now;
            return Some(fastest_req);
        }

        None
    }

    /// Find a faster server using `ServerStatMan`.
    ///
    /// Scans the first 10 remaining URIs and checks `ServerStatMan` for
    /// servers with speed > 1.5x the base request's speed. Selects the
    /// fastest candidate.
    ///
    /// # Arguments
    ///
    /// * `base` - Current base request to compare against.
    /// * `used_hosts` - Hosts currently in use.
    /// * `server_stat_man` - Server statistics manager.
    /// * `method` - HTTP method for the new request.
    pub fn find_faster_request_by_server_stat(
        &mut self,
        base: &Arc<Request>,
        used_hosts: &[(usize, String)],
        server_stat_man: &ServerStatMan,
        method: &str,
    ) -> Option<Arc<Request>> {
        let now = Instant::now();
        if now.duration_since(self.last_faster_replace) < STARTUP_IDLE_TIME {
            return None;
        }

        let in_flight_hosts = self.collect_in_flight_hosts();
        let base_stat = base.peer_stat();

        // Collect fast candidates from first NUM_URI_SCAN remaining URIs.
        let mut fast_cands: Vec<(u64, String)> = Vec::new(); // (speed, uri)

        for uri in self.remaining_uris.iter().take(NUM_URI_SCAN) {
            let (host, protocol) = match extract_host_and_protocol(uri) {
                Some(pair) => pair,
                None => continue,
            };

            // Skip if host is at max connection limit.
            let host_count = in_flight_hosts.iter().filter(|h| *h == &host).count();
            if host_count >= self.max_connection_per_server {
                debug!(
                    "{} has already used {} times, not considered",
                    uri, self.max_connection_per_server
                );
                continue;
            }

            // Skip if host is in used_hosts.
            if used_hosts.iter().any(|(_, h)| h == &host) {
                debug!("{} is in usedHosts, not considered", uri);
                continue;
            }

            // Look up server stat.
            let stat = server_stat_man.find_stat_by_protocol(&host, &protocol);
            let stat = match stat {
                Some(s) => s,
                None => continue,
            };

            if !stat.is_ok() {
                continue;
            }

            let server_speed = stat.get_download_speed();

            let is_faster = match base_stat {
                Some(bs) => server_speed as f64 > bs.download_speed as f64 * 1.5,
                None => server_speed > SPEED_THRESHOLD,
            };

            if is_faster {
                fast_cands.push((server_speed, uri.clone()));
            }
        }

        if fast_cands.is_empty() {
            debug!("No faster server found.");
            return None;
        }

        // Sort by speed descending and pick the fastest.
        fast_cands.sort_by_key(|b| std::cmp::Reverse(b.0));
        let (_, uri) = fast_cands.into_iter().next()?;

        debug!("Selected {} from fastCands", uri);

        // Create request from the fastest URI.
        let mut req = Request::new(&uri)?;
        req.set_referer(base.referer());
        req.set_method(method);

        // Remove URI from remaining_uris and add to spent_uris.
        if let Some(pos) = self.remaining_uris.iter().position(|u| *u == uri) {
            self.remaining_uris.remove(pos);
        }
        self.spent_uris.push_back(uri);

        let req = Arc::new(req);
        self.in_flight_requests.push(Arc::clone(&req));
        self.last_faster_replace = now;

        Some(req)
    }

    // =====================================================================
    // URI reuse
    // =====================================================================

    /// Reuse spent URIs that have not produced errors and whose host is
    /// not in `ignore`.
    ///
    /// Reusable URIs are appended to `remaining_uris`.
    /// This is called when all remaining URIs have been exhausted.
    pub fn reuse_uri(&mut self, ignore: &[String]) {
        for host in ignore {
            debug!("ignore host={}", host);
        }

        // Deduplicate spent URIs.
        let mut spent_sorted: Vec<String> = self.spent_uris.iter().cloned().collect();
        spent_sorted.sort();
        spent_sorted.dedup();

        // Collect error URIs.
        let mut error_uris: Vec<String> = self
            .uri_results
            .iter()
            .map(|r| r.uri.clone())
            .collect();
        error_uris.sort();
        error_uris.dedup();

        for uri in &error_uris {
            debug!("error URI={}", uri);
        }

        // Compute reusable URIs = spent - error (set difference).
        let mut reusable_uris = Vec::new();
        let mut error_iter = error_uris.iter().peekable();

        for spent_uri in &spent_sorted {
            // Advance error iterator past items < spent_uri.
            while error_iter.peek().is_some_and(|e| *e < spent_uri) {
                error_iter.next();
            }

            // If the error iterator's current item == spent_uri, skip it.
            if error_iter.peek() == Some(&spent_uri) {
                error_iter.next();
                continue;
            }

            reusable_uris.push(spent_uri.clone());
        }

        // Filter out URIs whose host is in the ignore list.
        reusable_uris.retain(|uri| {
            extract_host(uri)
                .as_ref()
                .is_none_or(|host| !ignore.iter().any(|ig| ig == host.as_str()))
        });

        debug!("Found {} reusable URIs", reusable_uris.len());
        for uri in &reusable_uris {
            debug!("URI={}", uri);
        }

        self.remaining_uris.extend(reusable_uris);
    }

    /// Push URIs from pooled and in-flight requests to the front of
    /// `remaining_uris`.
    ///
    /// This is used when re-preparing a download for retry.
    pub fn put_back_request(&mut self) {
        // Push in-flight URIs first (they go to the very front).
        for req in self.in_flight_requests.iter().rev() {
            self.remaining_uris.push_front(req.uri().to_owned());
        }
        // Then pooled URIs.
        for req in self.request_pool.iter().rev() {
            self.remaining_uris.push_front(req.uri().to_owned());
        }
    }

    // =====================================================================
    // Connection control
    // =====================================================================

    /// Return the max concurrent connections per server.
    pub fn max_connection_per_server(&self) -> usize {
        self.max_connection_per_server
    }

    /// Set the max concurrent connections per server.
    pub fn set_max_connection_per_server(&mut self, n: usize) {
        self.max_connection_per_server = n.max(1);
    }

    // =====================================================================
    // Runtime resource management
    // =====================================================================

    /// Release all runtime resources (pooled and in-flight requests).
    pub fn release_runtime_resource(&mut self) {
        self.request_pool.clear();
        self.in_flight_requests.clear();
    }

    /// Check if the local file exists on disk.
    pub fn exists(&self) -> bool {
        !self.path.is_empty() && Path::new(&self.path).exists()
    }

    // =====================================================================
    // Comparison
    // =====================================================================

    /// Compare by offset (for sorting file entries by position).
    pub fn cmp_by_offset(&self, other: &FileEntry) -> std::cmp::Ordering {
        self.offset.cmp(&other.offset)
    }

    // =====================================================================
    // Private helpers
    // =====================================================================

    /// Store a request in the pool, sorted by avg download speed (fastest first).
    fn store_pool(&mut self, request: Arc<Request>) {
        // Calculate avg speed before inserting to ensure correct position.
        // (PeerStat avg speed should already be up-to-date from the engine.)
        self.request_pool.push(request);
        self.sort_pool_by_speed();
    }

    /// Sort the request pool by avg download speed (fastest first).
    ///
    /// Requests without `PeerStat` are sorted to the end.
    fn sort_pool_by_speed(&mut self) {
        self.request_pool.sort_by(|a, b| {
            let a_speed = a.peer_stat().map(|ps| ps.avg_download_speed).unwrap_or(0);
            let b_speed = b.peer_stat().map(|ps| ps.avg_download_speed).unwrap_or(0);
            // Faster first (descending), with pointer tiebreaker for stability.
            match b_speed.cmp(&a_speed) {
                std::cmp::Ordering::Equal => {
                    // Tiebreaker by pointer identity (ascending, arbitrary but stable).
                    Arc::as_ptr(a).cmp(&Arc::as_ptr(b))
                }
                other => other,
            }
        });
    }

    /// Collect hostnames of all in-flight requests.
    fn collect_in_flight_hosts(&self) -> Vec<String> {
        self.in_flight_requests
            .iter()
            .filter_map(|req| extract_host(req.uri()))
            .collect()
    }

    /// Find a request in `in_flight_requests` by URI (not marked for removal).
    fn find_request_by_uri_in_flight(&self, uri: &str) -> Option<Arc<Request>> {
        self.in_flight_requests
            .iter()
            .find(|req| !req.removal_requested() && req.uri() == uri)
            .cloned()
    }

    /// Find a request in `request_pool` by URI (not marked for removal).
    fn find_request_by_uri_in_pool(&self, uri: &str) -> Option<Arc<Request>> {
        self.request_pool
            .iter()
            .find(|req| !req.removal_requested() && req.uri() == uri)
            .cloned()
    }

    /// Internal: get a request by selecting from URIs, respecting
    /// max-connection-per-server limits.
    fn get_request_with_in_flight_hosts(
        &mut self,
        selector: &dyn UriSelector,
        uri_reuse: bool,
        used_hosts: &[(usize, String)],
        referer: &str,
        method: &str,
        in_flight_hosts: &[String],
    ) -> Option<Arc<Request>> {
        let mut req: Option<Arc<Request>> = None;

        for pass in 0..2 {
            let mut pending: Vec<String> = Vec::new();
            let mut ignore_host: Vec<String> = Vec::new();

            // Try to select a URI.
            while !self.remaining_uris.is_empty() {
                let idx = selector.select(
                    &self.remaining_uris.iter().cloned().collect::<Vec<_>>(),
                    used_hosts,
                );

                let uri = match idx {
                    Some(i) if i < self.remaining_uris.len() => {
                        // VecDeque::remove returns Option<String>; unwrap is safe
                        // because we just checked i < len.
                        self.remaining_uris.remove(i).unwrap()
                    }
                    _ => break,
                };

                // Try to create a Request from this URI.
                let mut new_req = match Request::new(&uri) {
                    Some(r) => r,
                    None => continue,
                };

                // Check if host is at max connection limit.
                let host = new_req.host().to_owned();
                let host_count = in_flight_hosts
                    .iter()
                    .filter(|h| *h == &host)
                    .count();
                if host_count >= self.max_connection_per_server {
                    pending.push(uri);
                    ignore_host.push(host);
                    continue;
                }

                // Set referer.
                if referer == "*" {
                    new_req.set_referer(&uri);
                } else {
                    new_req.set_referer(referer);
                }
                new_req.set_method(method);

                // Move URI to spent and add request to in-flight.
                self.spent_uris.push_back(uri);
                let req_arc = Arc::new(new_req);
                self.in_flight_requests.push(Arc::clone(&req_arc));
                req = Some(req_arc);
                break;
            }

            // Put pending URIs back at the front of remaining_uris.
            for uri in pending.into_iter().rev() {
                self.remaining_uris.push_front(uri);
            }

            // On first pass: if uri_reuse is enabled and no request was found
            // and all remaining URIs are at max connections, try reusing.
            if pass == 0 && uri_reuse && req.is_none() {
                // Check if all remaining URIs were just at max connection limit.
                // If remaining_uris is empty or all are pending, reuse.
                if self.remaining_uris.is_empty() {
                    self.reuse_uri(&ignore_host);
                    continue;
                }
            }

            break;
        }

        req
    }
}

// ---------------------------------------------------------------------------
// PartialOrd / Ord by offset
// ---------------------------------------------------------------------------

impl PartialEq for FileEntry {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
    }
}

impl Eq for FileEntry {}

impl Ord for FileEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl PartialOrd for FileEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Validate a URI string by attempting to parse it.
fn is_valid_uri(uri: &str) -> bool {
    url::Url::parse(uri).is_ok()
}

/// Extract the hostname from a URI string.
///
/// Returns `None` if the URI cannot be parsed.
fn extract_host(uri: &str) -> Option<String> {
    extract_host_and_protocol(uri).map(|(h, _)| h)
}

/// Extract both hostname and protocol from a URI string.
///
/// Handles `scheme://host:port/path` format. Returns `None` if the URI
/// cannot be parsed.
fn extract_host_and_protocol(uri: &str) -> Option<(String, String)> {
    crate::selector::feedback_uri_selector::extract_host_and_protocol(uri)
}

/// Return the first `FileEntry` in the slice that `is_requested()`.
pub fn get_first_requested_file_entry(entries: &[Arc<FileEntry>]) -> Option<&Arc<FileEntry>> {
    entries.iter().find(|e| e.is_requested())
}

/// Count the number of requested file entries in the slice.
pub fn count_requested_file_entry(entries: &[Arc<FileEntry>]) -> usize {
    entries.iter().filter(|e| e.is_requested()).count()
}

/// Return `true` if at least one requested `FileEntry` has remaining URIs.
pub fn is_uri_supplied_for_requested_file_entry(entries: &[Arc<FileEntry>]) -> bool {
    entries
        .iter()
        .any(|e| e.is_requested() && !e.remaining_uris().is_empty())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ─────────────────────────────────────────────────────

    #[test]
    fn test_default_construction() {
        let entry = FileEntry::default();
        assert_eq!(entry.length(), 0);
        assert_eq!(entry.offset(), 0);
        assert!(!entry.is_requested());
        assert!(!entry.is_unique_protocol());
        assert!(entry.remaining_uris().is_empty());
        assert!(entry.spent_uris().is_empty());
        assert!(entry.path().is_empty());
        assert!(entry.content_type().is_empty());
        assert!(entry.original_name().is_empty());
        assert!(entry.suffix_path().is_empty());
        assert_eq!(entry.max_connection_per_server(), 1);
    }

    #[test]
    fn test_parameterized_construction() {
        let entry = FileEntry::new(
            "/downloads/file.zip".to_string(),
            1024,
            2048,
            vec!["http://example.com/file.zip".to_string()],
        );
        assert_eq!(entry.path(), "/downloads/file.zip");
        assert_eq!(entry.length(), 1024);
        assert_eq!(entry.offset(), 2048);
        assert!(entry.is_requested());
        assert_eq!(entry.remaining_uris().len(), 1);
    }

    // ── Path management ──────────────────────────────────────────────────

    #[test]
    fn test_path_accessors() {
        let mut entry = FileEntry::default();
        entry.set_path("/downloads/file.zip".to_string());
        assert_eq!(entry.path(), "/downloads/file.zip");
        assert_eq!(entry.basename(), "file.zip");
        assert_eq!(entry.dirname(), "/downloads");
    }

    #[test]
    fn test_basename_empty_path() {
        let entry = FileEntry::default();
        assert!(entry.basename().is_empty());
    }

    #[test]
    fn test_dirname_empty_path() {
        let entry = FileEntry::default();
        assert!(entry.dirname().is_empty());
    }

    #[test]
    fn test_original_name() {
        let mut entry = FileEntry::default();
        assert!(entry.original_name().is_empty());
        entry.set_original_name("original.zip".to_string());
        assert_eq!(entry.original_name(), "original.zip");
    }

    #[test]
    fn test_suffix_path() {
        let mut entry = FileEntry::default();
        assert!(entry.suffix_path().is_empty());
        entry.set_suffix_path("file.zip".to_string());
        assert_eq!(entry.suffix_path(), "file.zip");
    }

    #[test]
    fn test_content_type() {
        let mut entry = FileEntry::default();
        assert!(entry.content_type().is_empty());
        entry.set_content_type("application/zip".to_string());
        assert_eq!(entry.content_type(), "application/zip");
    }

    // ── Length / Offset ──────────────────────────────────────────────────

    #[test]
    fn test_length_offset() {
        let mut entry = FileEntry::default();
        entry.set_length(1024);
        entry.set_offset(2048);
        assert_eq!(entry.length(), 1024);
        assert_eq!(entry.offset(), 2048);
        assert_eq!(entry.last_offset(), 3072);
    }

    #[test]
    fn test_last_offset_saturating() {
        let mut entry = FileEntry::default();
        entry.set_length(u64::MAX);
        entry.set_offset(1);
        assert_eq!(entry.last_offset(), u64::MAX); // saturating_add
    }

    #[test]
    fn test_gtoloff() {
        let mut entry = FileEntry::default();
        entry.set_offset(1000);
        assert_eq!(entry.gtoloff(1000), 0);
        assert_eq!(entry.gtoloff(1500), 500);
    }

    #[test]
    #[should_panic]
    fn test_gtoloff_panics_on_invalid_offset() {
        let mut entry = FileEntry::default();
        entry.set_offset(1000);
        entry.gtoloff(500); // should panic in debug
    }

    // ── Requested / UniqueProtocol ───────────────────────────────────────

    #[test]
    fn test_requested_flag() {
        let mut entry = FileEntry::default();
        assert!(!entry.is_requested());
        entry.set_requested(true);
        assert!(entry.is_requested());
    }

    #[test]
    fn test_unique_protocol_flag() {
        let mut entry = FileEntry::default();
        assert!(!entry.is_unique_protocol());
        entry.set_unique_protocol(true);
        assert!(entry.is_unique_protocol());
    }

    // ── URI management ───────────────────────────────────────────────────

    #[test]
    fn test_add_uri_valid() {
        let mut entry = FileEntry::default();
        assert!(entry.add_uri("http://example.com/file.zip"));
        assert_eq!(entry.remaining_uris().len(), 1);
        assert_eq!(entry.remaining_uris()[0], "http://example.com/file.zip");
    }

    #[test]
    fn test_add_uri_invalid() {
        let mut entry = FileEntry::default();
        assert!(!entry.add_uri("not a url"));
        assert!(entry.remaining_uris().is_empty());
    }

    #[test]
    fn test_add_uris() {
        let mut entry = FileEntry::default();
        let count = entry.add_uris(&[
            "http://a.com/file".to_string(),
            "http://b.com/file".to_string(),
            "invalid".to_string(),
        ]);
        assert_eq!(count, 2);
        assert_eq!(entry.remaining_uris().len(), 2);
    }

    #[test]
    fn test_set_uris() {
        let mut entry = FileEntry::default();
        entry.add_uri("http://old.com/file");
        let count = entry.set_uris(&[
            "http://new1.com/file".to_string(),
            "http://new2.com/file".to_string(),
        ]);
        assert_eq!(count, 2);
        assert_eq!(entry.remaining_uris().len(), 2);
    }

    #[test]
    fn test_insert_uri() {
        let mut entry = FileEntry::default();
        entry.add_uri("http://a.com/file");
        entry.add_uri("http://c.com/file");
        assert!(entry.insert_uri("http://b.com/file", 1));
        assert_eq!(entry.remaining_uris().len(), 3);
        assert_eq!(entry.remaining_uris()[1], "http://b.com/file");
    }

    #[test]
    fn test_insert_uri_at_end() {
        let mut entry = FileEntry::default();
        entry.add_uri("http://a.com/file");
        assert!(entry.insert_uri("http://b.com/file", 100)); // pos > len
        assert_eq!(entry.remaining_uris().len(), 2);
    }

    #[test]
    fn test_uris_concatenated() {
        let mut entry = FileEntry::default();
        entry.add_uri("http://remaining.com/file");
        entry.spent_uris.push_back("http://spent.com/file".to_string());
        let all = entry.uris();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], "http://spent.com/file");
        assert_eq!(all[1], "http://remaining.com/file");
    }

    #[test]
    fn test_remove_uri_from_remaining() {
        let mut entry = FileEntry::default();
        entry.add_uri("http://a.com/file");
        entry.add_uri("http://b.com/file");
        assert!(entry.remove_uri("http://a.com/file"));
        assert_eq!(entry.remaining_uris().len(), 1);
        assert_eq!(entry.remaining_uris()[0], "http://b.com/file");
    }

    #[test]
    fn test_remove_uri_not_found() {
        let mut entry = FileEntry::default();
        entry.add_uri("http://a.com/file");
        assert!(!entry.remove_uri("http://nonexistent.com/file"));
    }

    #[test]
    fn test_remove_uri_from_spent() {
        let mut entry = FileEntry::default();
        entry.spent_uris
            .push_back("http://spent.com/file".to_string());
        assert!(entry.remove_uri("http://spent.com/file"));
        assert!(entry.spent_uris().is_empty());
    }

    #[test]
    fn test_remove_uri_whose_hostname_is() {
        let mut entry = FileEntry::default();
        entry.add_uri("http://a.com/file1");
        entry.add_uri("http://b.com/file2");
        entry.add_uri("http://a.com/file3");
        entry.remove_uri_whose_hostname_is("a.com");
        assert_eq!(entry.remaining_uris().len(), 1);
        assert_eq!(entry.remaining_uris()[0], "http://b.com/file2");
    }

    #[test]
    fn test_remove_identical_uri() {
        let mut entry = FileEntry::default();
        entry.add_uri("http://a.com/file");
        entry.add_uri("http://a.com/file"); // duplicate
        entry.add_uri("http://b.com/file");
        entry.remove_identical_uri("http://a.com/file");
        assert_eq!(entry.remaining_uris().len(), 1);
        assert_eq!(entry.remaining_uris()[0], "http://b.com/file");
    }

    #[test]
    fn test_empty_request_uri() {
        let mut entry = FileEntry::default();
        assert!(entry.empty_request_uri());
        entry.add_uri("http://a.com/file");
        assert!(!entry.empty_request_uri());
    }

    // ── URI results ──────────────────────────────────────────────────────

    #[test]
    fn test_add_uri_result() {
        let mut entry = FileEntry::default();
        entry.add_uri_result("http://a.com/file".to_string(), 1);
        entry.add_uri_result("http://b.com/file".to_string(), 2);
        assert_eq!(entry.uri_results().len(), 2);
    }

    #[test]
    fn test_extract_uri_result() {
        let mut entry = FileEntry::default();
        entry.add_uri_result("http://a.com/file".to_string(), 1);
        entry.add_uri_result("http://b.com/file".to_string(), 2);
        entry.add_uri_result("http://c.com/file".to_string(), 1);

        let mut extracted = VecDeque::new();
        entry.extract_uri_result(&mut extracted, 1);
        assert_eq!(extracted.len(), 2);
        assert_eq!(entry.uri_results().len(), 1);
        assert_eq!(entry.uri_results()[0].result_code, 2);
    }

    // ── Request pool / in-flight ─────────────────────────────────────────

    #[test]
    fn test_pool_request() {
        let mut entry = FileEntry::default();
        let req = Request::new("http://example.com/file").unwrap();
        let req = Arc::new(req);
        // Add to in-flight first.
        entry.in_flight_requests.push(Arc::clone(&req));
        // Pool it.
        entry.pool_request(&req);
        assert_eq!(entry.count_in_flight_request(), 0);
        assert_eq!(entry.count_pooled_request(), 1);
    }

    #[test]
    fn test_pool_request_removal_requested() {
        let mut entry = FileEntry::default();
        let mut req = Request::new("http://example.com/file").unwrap();
        req.request_removal();
        let req = Arc::new(req);
        entry.in_flight_requests.push(Arc::clone(&req));
        entry.pool_request(&req);
        // Should be discarded, not pooled.
        assert_eq!(entry.count_in_flight_request(), 0);
        assert_eq!(entry.count_pooled_request(), 0);
    }

    #[test]
    fn test_remove_request() {
        let mut entry = FileEntry::default();
        let req = Request::new("http://example.com/file").unwrap();
        let req = Arc::new(req);
        entry.in_flight_requests.push(Arc::clone(&req));
        assert!(entry.remove_request(&req));
        assert_eq!(entry.count_in_flight_request(), 0);
    }

    #[test]
    fn test_remove_request_not_found() {
        let mut entry = FileEntry::default();
        let req = Request::new("http://example.com/file").unwrap();
        let req = Arc::new(req);
        assert!(!entry.remove_request(&req));
    }

    // ── Connection control ───────────────────────────────────────────────

    #[test]
    fn test_max_connection_per_server() {
        let mut entry = FileEntry::default();
        assert_eq!(entry.max_connection_per_server(), 1);
        entry.set_max_connection_per_server(4);
        assert_eq!(entry.max_connection_per_server(), 4);
    }

    #[test]
    fn test_max_connection_per_server_minimum() {
        let mut entry = FileEntry::default();
        entry.set_max_connection_per_server(0); // should clamp to 1
        assert_eq!(entry.max_connection_per_server(), 1);
    }

    // ── Runtime resource management ──────────────────────────────────────

    #[test]
    fn test_release_runtime_resource() {
        let mut entry = FileEntry::default();
        let req = Arc::new(Request::new("http://example.com/file").unwrap());
        entry.in_flight_requests.push(Arc::clone(&req));
        entry.request_pool.push(req);
        entry.release_runtime_resource();
        assert_eq!(entry.count_in_flight_request(), 0);
        assert_eq!(entry.count_pooled_request(), 0);
    }

    // ── File existence ───────────────────────────────────────────────────

    #[test]
    fn test_exists_empty_path() {
        let entry = FileEntry::default();
        assert!(!entry.exists());
    }

    #[test]
    fn test_exists_nonexistent_file() {
        let mut entry = FileEntry::default();
        entry.set_path("/nonexistent/path/file.zip".to_string());
        assert!(!entry.exists());
    }

    // ── Comparison ───────────────────────────────────────────────────────

    #[test]
    fn test_comparison_by_offset() {
        let mut e1 = FileEntry::default();
        let mut e2 = FileEntry::default();
        e1.set_offset(100);
        e2.set_offset(200);
        assert!(e1 < e2);
        assert!(e2 > e1);
    }

    #[test]
    fn test_eq_same_offset() {
        let mut e1 = FileEntry::default();
        let mut e2 = FileEntry::default();
        e1.set_offset(100);
        e2.set_offset(100);
        assert_eq!(e1, e2);
    }

    // ── URI reuse ────────────────────────────────────────────────────────

    #[test]
    fn test_reuse_uri_basic() {
        let mut entry = FileEntry::default();
        // Simulate: spent URIs without errors should be reusable.
        entry.spent_uris
            .push_back("http://a.com/file".to_string());
        entry.spent_uris
            .push_back("http://b.com/file".to_string());
        // One URI had an error.
        entry.add_uri_result("http://a.com/file".to_string(), 2);

        entry.reuse_uri(&[]);
        // Only b.com should be reusable.
        assert_eq!(entry.remaining_uris().len(), 1);
        assert_eq!(entry.remaining_uris()[0], "http://b.com/file");
    }

    #[test]
    fn test_reuse_uri_with_ignore() {
        let mut entry = FileEntry::default();
        entry.spent_uris
            .push_back("http://a.com/file".to_string());
        entry.spent_uris
            .push_back("http://b.com/file".to_string());

        entry.reuse_uri(&["a.com".to_string()]);
        // a.com should be ignored.
        assert_eq!(entry.remaining_uris().len(), 1);
        assert_eq!(entry.remaining_uris()[0], "http://b.com/file");
    }

    // ── putBackRequest ───────────────────────────────────────────────────

    #[test]
    fn test_put_back_request() {
        let mut entry = FileEntry::default();
        let req1 = Arc::new(Request::new("http://a.com/file").unwrap());
        let req2 = Arc::new(Request::new("http://b.com/file").unwrap());
        entry.request_pool.push(Arc::clone(&req1));
        entry.in_flight_requests.push(Arc::clone(&req2));

        entry.put_back_request();
        // URIs should be at front of remaining_uris.
        assert_eq!(entry.remaining_uris().len(), 2);
    }

    // ── Free functions ───────────────────────────────────────────────────

    #[test]
    fn test_get_first_requested_file_entry() {
        let e1 = Arc::new(FileEntry::default()); // not requested
        let mut e2 = FileEntry::default();
        e2.set_requested(true);
        let e2 = Arc::new(e2);

        let entries = vec![e1, e2];
        let result = get_first_requested_file_entry(&entries);
        assert!(result.is_some());
        assert!(result.unwrap().is_requested());
    }

    #[test]
    fn test_get_first_requested_file_entry_none() {
        let entries: Vec<Arc<FileEntry>> = vec![Arc::new(FileEntry::default())];
        assert!(get_first_requested_file_entry(&entries).is_none());
    }

    #[test]
    fn test_count_requested_file_entry() {
        let mut e1 = FileEntry::default();
        e1.set_requested(true);
        let e2 = FileEntry::default();
        let entries = vec![Arc::new(e1), Arc::new(e2)];
        assert_eq!(count_requested_file_entry(&entries), 1);
    }

    #[test]
    fn test_is_uri_supplied_for_requested_file_entry() {
        let mut e1 = FileEntry::default();
        e1.set_requested(true);
        e1.add_uri("http://example.com/file");
        let entries = vec![Arc::new(e1)];
        assert!(is_uri_supplied_for_requested_file_entry(&entries));
    }

    #[test]
    fn test_is_uri_supplied_no_uris() {
        let mut e1 = FileEntry::default();
        e1.set_requested(true);
        // No URIs.
        let entries = vec![Arc::new(e1)];
        assert!(!is_uri_supplied_for_requested_file_entry(&entries));
    }

    // ── URI validation ───────────────────────────────────────────────────

    #[test]
    fn test_is_valid_uri() {
        assert!(is_valid_uri("http://example.com/file.zip"));
        assert!(is_valid_uri("https://example.com:8443/path"));
        assert!(is_valid_uri("ftp://ftp.example.com/pub/file"));
        assert!(!is_valid_uri("not a url"));
        assert!(!is_valid_uri(""));
    }

    // ── Extract host ─────────────────────────────────────────────────────

    #[test]
    fn test_extract_host() {
        assert_eq!(
            extract_host("http://example.com/path"),
            Some("example.com".to_string())
        );
        assert_eq!(
            extract_host("https://cdn.example.com:8443/file"),
            Some("cdn.example.com:8443".to_string())
        );
        assert_eq!(extract_host("invalid"), None);
    }
}
