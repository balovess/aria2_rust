//! URI management, URI results, and URI reuse operations for FileEntry.

use std::collections::VecDeque;
use std::sync::Arc;

use tracing::debug;

use super::entry::FileEntry;
use super::helpers::{extract_host, is_valid_uri};
use super::types::UriResult;

const MAX_URI_RESULTS: usize = 64;

// ============================================================================
// URI management
// ============================================================================

impl FileEntry {
    /// Return an owned snapshot of the remaining (not-yet-dispatched) URIs.
    pub fn remaining_uris(&self) -> VecDeque<String> {
        self.read_uri_state().remaining.clone()
    }

    /// Return an owned snapshot of the spent (already-dispatched) URIs.
    pub fn spent_uris(&self) -> VecDeque<String> {
        self.read_uri_state().spent.clone()
    }

    /// Return the three URI lifecycle queues from one consistent snapshot.
    pub fn uri_state_snapshot(&self) -> (VecDeque<String>, VecDeque<String>, VecDeque<UriResult>) {
        let state = self.read_uri_state();
        (
            state.remaining.clone(),
            state.spent.clone(),
            state.results.clone(),
        )
    }

    /// Return whether this file has any configured or previously dispatched URI.
    pub fn has_uri_sources(&self) -> bool {
        let state = self.read_uri_state();
        !state.remaining.is_empty() || !state.spent.is_empty()
    }

    pub(crate) fn visit_uri_storage(&self, mut visit: impl FnMut(&str, usize)) {
        let state = self.read_uri_state();
        for uri in state.remaining.iter().chain(state.spent.iter()) {
            visit(uri, uri.capacity());
        }
        for result in &state.results {
            visit(&result.uri, result.uri.capacity());
        }
    }

    /// Return all URIs (spent + remaining) as a single vector.
    pub fn uris(&self) -> Vec<String> {
        let state = self.read_uri_state();
        state
            .spent
            .iter()
            .chain(state.remaining.iter())
            .cloned()
            .collect()
    }

    /// Replace all remaining URIs with the given list.
    ///
    /// Returns the number of valid URIs added.
    pub fn set_uris(&mut self, uris: &[String]) -> usize {
        let mut state = self.write_uri_state();
        state.remaining.clear();
        uris.iter()
            .filter(|uri| is_valid_uri(uri))
            .map(|uri| {
                state.remaining.push_back(uri.clone());
                1
            })
            .sum()
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
            self.write_uri_state().remaining.push_back(uri.to_owned());
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
        let mut state = self.write_uri_state();
        let remaining = &mut state.remaining;
        let insert_pos = pos.min(remaining.len());
        // VecDeque doesn't have a direct insert; convert if needed.
        if insert_pos == remaining.len() {
            remaining.push_back(uri.to_owned());
        } else if insert_pos == 0 {
            remaining.push_front(uri.to_owned());
        } else {
            // Split and reassemble for mid-deque insertion.
            let mut right = remaining.split_off(insert_pos);
            remaining.push_back(uri.to_owned());
            remaining.append(&mut right);
        }
        true
    }

    /// Remove a URI from `remaining_uris` or `spent_uris`.
    ///
    /// If the URI is in `spent_uris`, any corresponding in-flight or pooled
    /// request is marked for removal. Returns `true` if the URI was found.
    pub fn remove_uri(&mut self, uri: &str) -> bool {
        let (removed, was_spent) = remove_uri_from_state(&mut self.write_uri_state(), uri);
        if !removed {
            return false;
        }
        if was_spent {
            let req = self
                .find_request_by_uri_in_flight(uri)
                .or_else(|| self.find_request_by_uri_in_pool(uri));
            if let Some(req) = req
                && let Some(pos) = self.request_pool.iter().position(|r| Arc::ptr_eq(r, &req))
            {
                self.request_pool.remove(pos);
            }
        }
        true
    }

    /// Change a file's URI queues through a shared `DownloadContext`.
    pub(crate) fn change_uris_shared(
        &self,
        del_uris: &[String],
        add_uris: &[String],
        position: Option<usize>,
    ) -> (usize, usize) {
        let mut state = self.write_uri_state();
        let deleted = del_uris
            .iter()
            .filter(|uri| remove_uri_from_state(&mut state, uri).0)
            .count();
        let mut added = 0;
        let mut insertion = position.unwrap_or(state.remaining.len());
        for uri in add_uris {
            if !is_valid_uri(uri) {
                continue;
            }
            let index = insertion.min(state.remaining.len());
            state.remaining.insert(index, uri.clone());
            insertion = index.saturating_add(1);
            added += 1;
        }
        (deleted, added)
    }

    /// Remove a URI from `remaining_uris` or `spent_uris`, and mark any
    /// associated request for removal.
    ///
    /// This is the full-featured version that handles marking requests.
    /// Returns `true` if the URI was found.
    pub fn remove_uri_and_mark(&mut self, uri: &str) -> bool {
        let (removed, was_spent) = remove_uri_from_state(&mut self.write_uri_state(), uri);
        if !removed {
            return false;
        }
        if was_spent {
            if let Some(req) = self.find_request_by_uri_in_flight(uri) {
                if let Some(pool_pos) = self.request_pool.iter().position(|r| Arc::ptr_eq(r, &req))
                {
                    self.request_pool.remove(pool_pos);
                }
            } else if let Some(req) = self.find_request_by_uri_in_pool(uri)
                && let Some(pool_pos) = self.request_pool.iter().position(|r| Arc::ptr_eq(r, &req))
            {
                self.request_pool.remove(pool_pos);
            }
        }
        true
    }

    /// Remove all remaining URIs whose hostname matches the given hostname.
    pub fn remove_uri_whose_hostname_is(&mut self, hostname: &str) {
        let mut state = self.write_uri_state();
        let before = state.remaining.len();
        state
            .remaining
            .retain(|uri| extract_host(uri).as_deref() != Some(hostname));
        let removed = before - state.remaining.len();
        if removed > 0 {
            debug!(
                "Removed {} URIs with hostname '{}' for path={}",
                removed, hostname, self.path
            );
        }
    }

    /// Remove all occurrences of `uri` from `remaining_uris`.
    pub fn remove_identical_uri(&mut self, uri: &str) {
        self.write_uri_state().remaining.retain(|u| u != uri);
    }

    /// Return `true` if there are no remaining URIs, in-flight requests,
    /// or pooled requests.
    pub fn empty_request_uri(&self) -> bool {
        self.read_uri_state().remaining.is_empty()
            && self.in_flight_requests.is_empty()
            && self.request_pool.is_empty()
    }
}

// ============================================================================
// URI results
// ============================================================================

impl FileEntry {
    /// Add a URI result record.
    pub fn add_uri_result(&mut self, uri: String, result_code: u16) {
        let mut state = self.write_uri_state();
        state.results.push_back(UriResult::new(uri, result_code));
        if state.results.len() > MAX_URI_RESULTS {
            state.results.pop_front();
        }
    }

    /// Return an owned snapshot of the URI results.
    pub fn uri_results(&self) -> VecDeque<UriResult> {
        self.read_uri_state().results.clone()
    }

    /// Extract URI results matching `result_code`, removing them from
    /// `uri_results`.
    ///
    /// Matching results are moved into `res`. Non-matching results remain
    /// in `uri_results` in their original order.
    pub fn extract_uri_result(&mut self, res: &mut VecDeque<UriResult>, result_code: u16) {
        let mut state = self.write_uri_state();
        // Partition: matching results go to `res`, non-matching stay.
        let mut matching = VecDeque::new();
        let mut non_matching = VecDeque::new();

        for ur in state.results.drain(..) {
            if ur.result_code == result_code {
                matching.push_back(ur);
            } else {
                non_matching.push_back(ur);
            }
        }

        res.extend(matching);
        state.results = non_matching;
    }
}

// ============================================================================
// URI reuse
// ============================================================================

impl FileEntry {
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
        let mut state = self.write_uri_state();
        let mut spent_sorted: Vec<String> = state.spent.iter().cloned().collect();
        spent_sorted.sort();
        spent_sorted.dedup();

        // Collect error URIs.
        let mut error_uris: Vec<String> = state.results.iter().map(|r| r.uri.clone()).collect();
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

        state.remaining.extend(reusable_uris);
    }

    /// Push URIs from pooled and in-flight requests to the front of
    /// `remaining_uris`.
    ///
    /// This is used when re-preparing a download for retry.
    pub fn put_back_request(&mut self) {
        let mut state = self.write_uri_state();
        // Push in-flight URIs first (they go to the very front).
        for req in self.in_flight_requests.iter().rev() {
            state.remaining.push_front(req.uri().to_owned());
        }
        // Then pooled URIs.
        for req in self.request_pool.iter().rev() {
            state.remaining.push_front(req.uri().to_owned());
        }
    }
}

fn remove_uri_from_state(state: &mut super::entry::UriState, uri: &str) -> (bool, bool) {
    if let Some(pos) = state.remaining.iter().position(|value| value == uri) {
        state.remaining.remove(pos);
        return (true, false);
    }
    if let Some(pos) = state.spent.iter().position(|value| value == uri) {
        state.spent.remove(pos);
        return (true, true);
    }
    (false, false)
}
