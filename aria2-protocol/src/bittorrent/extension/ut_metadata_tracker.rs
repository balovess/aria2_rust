//! UT metadata request timeout tracker (BEP 9).
//!
//! Tracks in-flight ut_metadata requests and removes those that have timed out.
//! This is the Rust equivalent of the C++ `UTMetadataRequestTracker` class.
//!
//! # C++ Reference
//!
//! `aria2_original/src/UTMetadataRequestTracker.h`
//! `aria2_original/src/UTMetadataRequestTracker.cc`
//!
//! The C++ version uses `wallclock` for elapsed-time checks. In Rust we use
//! `std::time::Instant` which is monotonic and unaffected by system clock
//! changes — a strictly better choice for timeout tracking.
//!
//! # Key Differences from C++
//!
//! - C++ uses `global::wallclock()` (mockable system clock). Rust uses
//!   `Instant::now()` (monotonic, not mockable but correct for timeouts).
//! - C++ timeout is 20 seconds (`20_s`). Rust uses the same 20-second default.
//! - C++ `MAX_OUTSTANDING_REQUEST = 1`. Rust uses the same constant.
//! - Rust provides `request_piece()` as a convenience that combines `avail()`
//!   check + `add()`, since the C++ call site always checks `avail()` first.

use std::time::{Duration, Instant};

/// Default timeout for ut_metadata requests.
///
/// C++: `constexpr auto TIMEOUT = 20_s;`
pub const UT_METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Maximum number of outstanding ut_metadata requests.
///
/// C++: `const size_t MAX_OUTSTANDING_REQUEST = 1;`
///
/// Only one metadata piece request may be in flight at a time. This prevents
/// overwhelming peers with metadata requests and ensures orderly piece
/// collection.
pub const MAX_OUTSTANDING_REQUESTS: usize = 1;

// ---------------------------------------------------------------------------
// RequestEntry
// ---------------------------------------------------------------------------

/// A single tracked metadata piece request.
///
/// C++: `UTMetadataRequestTracker::RequestEntry`
#[derive(Debug)]
struct RequestEntry {
    /// The metadata piece index being requested.
    index: u32,
    /// When this request was dispatched.
    dispatched_at: Instant,
}

impl RequestEntry {
    fn new(index: u32) -> Self {
        Self {
            index,
            dispatched_at: Instant::now(),
        }
    }

    /// Returns `true` if this request has been in flight for longer than
    /// `timeout`.
    ///
    /// C++: `RequestEntry::elapsed(t)`
    fn is_timed_out(&self, timeout: Duration) -> bool {
        self.dispatched_at.elapsed() >= timeout
    }
}

impl PartialEq for RequestEntry {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

// ---------------------------------------------------------------------------
// UTMetadataRequestTracker
// ---------------------------------------------------------------------------

/// Tracks in-flight ut_metadata piece requests and detects timeouts.
///
/// The tracker limits the number of concurrent metadata requests to
/// [`MAX_OUTSTANDING_REQUESTS`] (1 by default). When a request times out
/// (default 20 seconds), it is automatically removed and its index returned
/// so the caller can retry.
///
/// # Usage
///
/// ```ignore
/// let mut tracker = UTMetadataRequestTracker::new();
///
/// // Before requesting a piece, check if we have capacity
/// if tracker.avail() > 0 {
///     tracker.add(0); // request piece 0
/// }
///
/// // When a response arrives, remove the tracked entry
/// tracker.remove(0);
///
/// // Periodically, check for timeouts
/// let timed_out = tracker.remove_timeout_entries();
/// for index in timed_out {
///     // Re-request piece `index`
/// }
/// ```
///
/// # C++ Reference
///
/// `UTMetadataRequestTracker` in `aria2_original/src/UTMetadataRequestTracker.{h,cc}`
#[derive(Debug)]
pub struct UTMetadataRequestTracker {
    /// Currently tracked in-flight requests.
    tracked: Vec<RequestEntry>,
    /// Timeout duration for each request.
    timeout: Duration,
}

impl UTMetadataRequestTracker {
    /// Create a new tracker with the default 20-second timeout.
    pub fn new() -> Self {
        Self {
            tracked: Vec::new(),
            timeout: UT_METADATA_REQUEST_TIMEOUT,
        }
    }

    /// Create a new tracker with a custom timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            tracked: Vec::new(),
            timeout,
        }
    }

    /// Add a metadata piece index to the tracking list.
    ///
    /// C++: `UTMetadataRequestTracker::add(size_t index)`
    ///
    /// # Panics
    ///
    /// Does not panic, but callers should check `avail()` first to avoid
    /// exceeding [`MAX_OUTSTANDING_REQUESTS`].
    pub fn add(&mut self, index: u32) {
        self.tracked.push(RequestEntry::new(index));
    }

    /// Returns `true` if the given piece index is currently being tracked.
    ///
    /// C++: `UTMetadataRequestTracker::tracks(size_t index)`
    pub fn tracks(&self, index: u32) -> bool {
        self.tracked.iter().any(|e| e.index == index)
    }

    /// Remove a piece index from the tracking list.
    ///
    /// C++: `UTMetadataRequestTracker::remove(size_t index)`
    ///
    /// Does nothing if the index is not currently tracked.
    pub fn remove(&mut self, index: u32) {
        self.tracked.retain(|e| e.index != index);
    }

    /// Returns all currently tracked piece indexes.
    ///
    /// C++: `UTMetadataRequestTracker::getAllTrackedIndex()`
    pub fn all_tracked_indices(&self) -> Vec<u32> {
        self.tracked.iter().map(|e| e.index).collect()
    }

    /// Remove all timed-out entries and return their piece indexes.
    ///
    /// C++: `UTMetadataRequestTracker::removeTimeoutEntry()`
    ///
    /// The returned indexes should be re-requested by the caller.
    pub fn remove_timeout_entries(&mut self) -> Vec<u32> {
        let mut timed_out = Vec::new();

        self.tracked.retain(|e| {
            if e.is_timed_out(self.timeout) {
                tracing::debug!(index = e.index, "ut_metadata request timed out");
                timed_out.push(e.index);
                false
            } else {
                true
            }
        });

        timed_out
    }

    /// Returns the number of currently tracked requests.
    ///
    /// C++: `UTMetadataRequestTracker::count()`
    pub fn count(&self) -> usize {
        self.tracked.len()
    }

    /// Returns the number of additional requests this tracker can accept.
    ///
    /// C++: `UTMetadataRequestTracker::avail()`
    ///
    /// Returns 0 if already at [`MAX_OUTSTANDING_REQUESTS`].
    pub fn avail(&self) -> usize {
        MAX_OUTSTANDING_REQUESTS.saturating_sub(self.tracked.len())
    }

    /// Convenience: request a piece if capacity is available.
    ///
    /// Returns `true` if the piece was added to tracking, `false` if the
    /// tracker is at capacity (caller should wait for a response or timeout).
    ///
    /// This is a Rust-specific convenience that combines `avail()` + `add()`,
    /// since the C++ call site always checks `avail()` first:
    /// ```cpp
    /// if (tracker->avail() > 0) {
    ///     tracker->add(pieceIndex);
    ///     // send request...
    /// }
    /// ```
    pub fn request_piece(&mut self, index: u32) -> bool {
        if self.avail() > 0 {
            self.add(index);
            true
        } else {
            false
        }
    }

    /// Check if any tracked requests have timed out, without removing them.
    ///
    /// Useful for polling-based architectures where removal must happen
    /// at a specific point in the event loop.
    pub fn has_timeouts(&self) -> bool {
        self.tracked.iter().any(|e| e.is_timed_out(self.timeout))
    }
}

impl Default for UTMetadataRequestTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_tracker_is_empty() {
        let tracker = UTMetadataRequestTracker::new();
        assert_eq!(tracker.count(), 0);
        assert_eq!(tracker.avail(), MAX_OUTSTANDING_REQUESTS);
        assert!(!tracker.tracks(0));
        assert!(tracker.all_tracked_indices().is_empty());
    }

    #[test]
    fn test_add_and_tracks() {
        let mut tracker = UTMetadataRequestTracker::new();
        tracker.add(0);
        assert!(tracker.tracks(0));
        assert_eq!(tracker.count(), 1);
        assert!(!tracker.tracks(1));
    }

    #[test]
    fn test_remove() {
        let mut tracker = UTMetadataRequestTracker::new();
        tracker.add(0);
        tracker.add(1);
        assert_eq!(tracker.count(), 2);

        tracker.remove(0);
        assert!(!tracker.tracks(0));
        assert!(tracker.tracks(1));
        assert_eq!(tracker.count(), 1);
    }

    #[test]
    fn test_remove_nonexistent() {
        let mut tracker = UTMetadataRequestTracker::new();
        tracker.add(0);
        tracker.remove(99); // should not panic
        assert_eq!(tracker.count(), 1);
    }

    #[test]
    fn test_avail_limits() {
        let mut tracker = UTMetadataRequestTracker::new();
        assert_eq!(tracker.avail(), 1);

        tracker.add(0);
        assert_eq!(tracker.avail(), 0);

        // Adding beyond limit does not auto-reject in add()
        tracker.add(1);
        assert_eq!(tracker.avail(), 0); // saturating_sub returns 0
    }

    #[test]
    fn test_request_piece() {
        let mut tracker = UTMetadataRequestTracker::new();
        assert!(tracker.request_piece(0));
        assert!(!tracker.request_piece(1)); // at capacity
        assert!(tracker.tracks(0));
        assert!(!tracker.tracks(1));
    }

    #[test]
    fn test_all_tracked_indices() {
        let mut tracker = UTMetadataRequestTracker::new();
        tracker.add(2);
        tracker.add(5);
        let indices = tracker.all_tracked_indices();
        assert!(indices.contains(&2));
        assert!(indices.contains(&5));
    }

    #[test]
    fn test_timeout_with_custom_duration() {
        let mut tracker = UTMetadataRequestTracker::with_timeout(Duration::from_millis(1));
        tracker.add(0);
        assert!(!tracker.has_timeouts());

        // Spin briefly to ensure the instant advances
        std::thread::sleep(Duration::from_millis(5));

        assert!(tracker.has_timeouts());
        let timed_out = tracker.remove_timeout_entries();
        assert_eq!(timed_out, vec![0]);
        assert_eq!(tracker.count(), 0);
    }

    #[test]
    fn test_remove_timeout_preserves_not_timed_out() {
        let mut tracker = UTMetadataRequestTracker::with_timeout(Duration::from_millis(1));
        tracker.add(0);
        std::thread::sleep(Duration::from_millis(5));

        // Add a fresh entry that hasn't timed out yet
        tracker.add(1);
        let timed_out = tracker.remove_timeout_entries();

        assert_eq!(timed_out, vec![0]);
        assert!(tracker.tracks(1));
        assert_eq!(tracker.count(), 1);
    }

    #[test]
    fn test_default_trait() {
        let tracker = UTMetadataRequestTracker::default();
        assert_eq!(tracker.count(), 0);
    }
}
