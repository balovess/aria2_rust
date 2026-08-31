//! Request lifecycle, faster server detection, and private request helpers for FileEntry.

use std::sync::Arc;
use std::time::Instant;

use tracing::debug;

use super::entry::FileEntry;
use super::helpers::{extract_host, extract_host_and_protocol};
use super::types::{NUM_URI_SCAN, SPEED_THRESHOLD, STARTUP_IDLE_TIME};
use crate::download::request::Request;
use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

// ============================================================================
// Request lifecycle
// ============================================================================

impl FileEntry {
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
                    let first_pool_uri = self.request_pool.first().map(|r| r.uri().to_owned());
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
}

// ============================================================================
// Faster server detection
// ============================================================================

impl FileEntry {
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
        // A URI can be reused after a retry cycle. Keep spent URIs as a set
        // represented by the existing deque; repeated attempts must not grow
        // this history without bound.
        if !self.spent_uris.iter().any(|spent| spent == &uri) {
            self.spent_uris.push_back(uri);
        }

        let req = Arc::new(req);
        self.in_flight_requests.push(Arc::clone(&req));
        self.last_faster_replace = now;

        Some(req)
    }
}

// ============================================================================
// Private helpers
// ============================================================================

impl FileEntry {
    /// Store a request in the pool, sorted by avg download speed (fastest first).
    pub(super) fn store_pool(&mut self, request: Arc<Request>) {
        // Calculate avg speed before inserting to ensure correct position.
        // (PeerStat avg speed should already be up-to-date from the engine.)
        self.request_pool.push(request);
        self.sort_pool_by_speed();
    }

    /// Sort the request pool by avg download speed (fastest first).
    ///
    /// Requests without `PeerStat` are sorted to the end.
    pub(super) fn sort_pool_by_speed(&mut self) {
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
    pub(super) fn collect_in_flight_hosts(&self) -> Vec<String> {
        self.in_flight_requests
            .iter()
            .filter_map(|req| extract_host(req.uri()))
            .collect()
    }

    /// Find a request in `in_flight_requests` by URI (not marked for removal).
    pub(super) fn find_request_by_uri_in_flight(&self, uri: &str) -> Option<Arc<Request>> {
        self.in_flight_requests
            .iter()
            .find(|req| !req.removal_requested() && req.uri() == uri)
            .cloned()
    }

    /// Find a request in `request_pool` by URI (not marked for removal).
    pub(super) fn find_request_by_uri_in_pool(&self, uri: &str) -> Option<Arc<Request>> {
        self.request_pool
            .iter()
            .find(|req| !req.removal_requested() && req.uri() == uri)
            .cloned()
    }

    /// Internal: get a request by selecting from URIs, respecting
    /// max-connection-per-server limits.
    pub(super) fn get_request_with_in_flight_hosts(
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
                let host_count = in_flight_hosts.iter().filter(|h| *h == &host).count();
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
