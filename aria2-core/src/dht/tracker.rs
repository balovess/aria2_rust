//! DHT message tracker for matching queries to responses.
//!
//! Tracks outbound DHT queries by transaction ID, matching incoming
//! responses to their original queries. Handles timeout detection
//! and cleanup.
//!
//! # Design
//!
//! The C++ implementation uses inheritance-based `DHTMessageCallback`
//! for response handling. This Rust version decouples the tracker from
//! the routing table and message factory: `match_response()` returns a
//! [`MatchResult`] containing the method name and target node info, which
//! the caller uses to route the response appropriately. Similarly,
//! `handle_timeout()` returns timed-out entries for the caller to process
//! (e.g., update node RTT, mark nodes bad, drop from routing table).
//!
//! # C++ Reference
//!
//! - `DHTMessageTracker.h/cc` -> [`DhtMessageTracker`]
//! - `DHTMessageTrackerEntry.h/cc` -> [`TrackerEntry`]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tracing::{debug, trace, warn};

use super::constants::MESSAGE_TIMEOUT_SECS;
use super::node_id::NodeId;

// ---------------------------------------------------------------------------
// TrackerEntry
// ---------------------------------------------------------------------------

/// A tracked outbound DHT query entry.
///
/// Maps a transaction ID to the original query context, enabling
/// response matching and timeout detection.
///
/// C++: `DHTMessageTrackerEntry`
pub struct TrackerEntry {
    /// The target node's ID (may be zero-ID if unknown at query time).
    target_node_id: NodeId,
    /// The target node's address.
    target_addr: SocketAddr,
    /// The transaction ID of the outbound query.
    transaction_id: Vec<u8>,
    /// The DHT method name (e.g., "ping", "find_node", "get_peers", "announce_peer").
    method: String,
    /// When the query was dispatched.
    dispatched_at: Instant,
    /// Timeout duration for this query.
    timeout: Duration,
}

impl TrackerEntry {
    /// Create a new tracker entry.
    pub fn new(
        target_node_id: NodeId,
        target_addr: SocketAddr,
        transaction_id: Vec<u8>,
        method: String,
        timeout: Duration,
    ) -> Self {
        Self {
            target_node_id,
            target_addr,
            transaction_id,
            method,
            dispatched_at: Instant::now(),
            timeout,
        }
    }

    /// Check if this entry has timed out.
    ///
    /// C++: `DHTMessageTrackerEntry::isTimeout()`
    pub fn is_timeout(&self) -> bool {
        self.dispatched_at.elapsed() >= self.timeout
    }

    /// Extend the timeout by resetting the dispatch time to now.
    ///
    /// Note: The C++ `extendTimeout()` is a no-op, but we implement it
    /// as a reset since the method exists and future callers may need it.
    pub fn extend_timeout(&mut self) {
        self.dispatched_at = Instant::now();
    }

    /// Check if an incoming response matches this entry.
    ///
    /// Matches by transaction ID and sender address. The C++ version
    /// also handles IPv4-mapped IPv6 addresses; in Rust, `SocketAddr`
    /// comparison handles this directly since we store the address as
    /// seen when dispatching the query.
    ///
    /// C++: `DHTMessageTrackerEntry::match()`
    pub fn matches(&self, transaction_id: &[u8], sender_addr: &SocketAddr) -> bool {
        self.transaction_id == transaction_id && self.target_addr == *sender_addr
    }

    /// Get the method name for this tracked query.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Get the target node's ID.
    pub fn target_node_id(&self) -> &NodeId {
        &self.target_node_id
    }

    /// Get the target address.
    pub fn target_addr(&self) -> SocketAddr {
        self.target_addr
    }

    /// Get the transaction ID.
    pub fn transaction_id(&self) -> &[u8] {
        &self.transaction_id
    }

    /// Get elapsed time since dispatch.
    ///
    /// C++: `DHTMessageTrackerEntry::getElapsed()`
    pub fn elapsed(&self) -> Duration {
        self.dispatched_at.elapsed()
    }
}

// ---------------------------------------------------------------------------
// MatchResult
// ---------------------------------------------------------------------------

/// Result of matching an incoming response to a tracked query.
///
/// The caller uses this information to route the response to the
/// appropriate handler (replaces the C++ callback mechanism).
pub struct MatchResult {
    /// The method name of the original query.
    pub method: String,
    /// The target node's ID from the tracked query.
    pub target_node_id: NodeId,
    /// The target address from the tracked query.
    pub target_addr: SocketAddr,
    /// Elapsed time since the query was dispatched (for RTT update).
    ///
    /// C++: `DHTMessageTracker::messageArrived()` computes
    /// `entry->getElapsed()` and calls `node->updateRTT(rtt)`.
    pub elapsed: Duration,
}

// ---------------------------------------------------------------------------
// TimeoutEntry
// ---------------------------------------------------------------------------

/// Information about a timed-out query, returned to the caller for processing.
///
/// The caller should:
/// - Update the node's RTT from `elapsed`
/// - Call `node.timeout()` to increment the failure counter
/// - Drop the node from the routing table if it becomes bad
/// - Notify any waiting tasks of the timeout
pub struct TimeoutEntry {
    /// The target node's ID.
    pub target_node_id: NodeId,
    /// The target address.
    pub target_addr: SocketAddr,
    /// The DHT method name of the timed-out query.
    pub method: String,
    /// Elapsed time since dispatch (useful for RTT estimation on failure).
    pub elapsed: Duration,
}

// ---------------------------------------------------------------------------
// DhtMessageTracker
// ---------------------------------------------------------------------------

/// DHT message tracker for query/response matching.
///
/// Maintains a deque of tracker entries. When a query is sent,
/// it is registered via `add_query()`. When a response arrives,
/// `match_response()` is called to find the matching entry.
/// Timed-out entries are cleaned up via `handle_timeout()`.
///
/// C++: `DHTMessageTracker`
pub struct DhtMessageTracker {
    entries: VecDeque<TrackerEntry>,
    default_timeout: Duration,
}

impl DhtMessageTracker {
    /// Create a new tracker with the default message timeout.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            default_timeout: Duration::from_secs(MESSAGE_TIMEOUT_SECS),
        }
    }

    /// Create a new tracker with a custom default timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            default_timeout: timeout,
        }
    }

    /// Register a new outbound DHT query using the default timeout.
    ///
    /// C++: `DHTMessageTracker::addMessage()` (without custom callback)
    pub fn add_query(
        &mut self,
        target_node_id: NodeId,
        target_addr: SocketAddr,
        transaction_id: Vec<u8>,
        method: String,
    ) {
        self.add_query_with_timeout(
            target_node_id,
            target_addr,
            transaction_id,
            method,
            self.default_timeout,
        );
    }

    /// Register a new outbound DHT query with a custom timeout.
    ///
    /// C++: `DHTMessageTracker::addMessage()` (with custom timeout)
    pub fn add_query_with_timeout(
        &mut self,
        target_node_id: NodeId,
        target_addr: SocketAddr,
        transaction_id: Vec<u8>,
        method: String,
        timeout: Duration,
    ) {
        trace!(
            tid = ?transaction_id,
            method = %method,
            addr = %target_addr,
            "Tracking DHT query"
        );
        let entry = TrackerEntry::new(target_node_id, target_addr, transaction_id, method, timeout);
        self.entries.push_back(entry);
    }

    /// Try to match an incoming response to a tracked query.
    ///
    /// Searches entries for a match by transaction ID and sender address.
    /// If found, removes the entry and returns the match result.
    /// If not found, returns `None`.
    ///
    /// C++: `DHTMessageTracker::messageArrived()` (simplified — the C++
    /// version also creates a response message via the factory and handles
    /// node ID changes; here we return the match data for the caller to
    /// process).
    pub fn match_response(
        &mut self,
        transaction_id: &[u8],
        sender_addr: &SocketAddr,
    ) -> Option<MatchResult> {
        let pos = self
            .entries
            .iter()
            .position(|e| e.matches(transaction_id, sender_addr))?;
        let entry = self.entries.remove(pos)?;
        let elapsed = entry.elapsed();

        debug!(
            tid = ?transaction_id,
            method = %entry.method,
            addr = %sender_addr,
            elapsed_ms = elapsed.as_millis(),
            "DHT response matched to query"
        );

        Some(MatchResult {
            method: entry.method,
            target_node_id: entry.target_node_id,
            target_addr: entry.target_addr,
            elapsed,
        })
    }

    /// Handle timed-out entries.
    ///
    /// Removes entries whose timeout has expired and returns
    /// the list of [`TimeoutEntry`] for the caller to process
    /// (e.g., update node RTT, mark nodes bad, drop from routing table).
    ///
    /// C++: `DHTMessageTracker::handleTimeout()`
    pub fn handle_timeout(&mut self) -> Vec<TimeoutEntry> {
        let mut timed_out = Vec::new();
        let mut remaining = VecDeque::with_capacity(self.entries.len());

        while let Some(entry) = self.entries.pop_front() {
            if entry.is_timeout() {
                warn!(
                    tid = ?entry.transaction_id,
                    method = %entry.method,
                    addr = %entry.target_addr,
                    elapsed = ?entry.elapsed(),
                    "DHT query timed out"
                );
                let elapsed = entry.elapsed();
                timed_out.push(TimeoutEntry {
                    target_node_id: entry.target_node_id,
                    target_addr: entry.target_addr,
                    method: entry.method,
                    elapsed,
                });
            } else {
                remaining.push_back(entry);
            }
        }

        self.entries = remaining;
        timed_out
    }

    /// Get the number of tracked entries.
    ///
    /// C++: `DHTMessageTracker::countEntry()`
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Check if there are any tracked entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up an entry by transaction ID.
    ///
    /// C++: `DHTMessageTracker::getEntryFor()` (test-only in C++ as well)
    pub fn get_entry(&self, transaction_id: &[u8]) -> Option<&TrackerEntry> {
        self.entries
            .iter()
            .find(|e| e.transaction_id == transaction_id)
    }
}

impl Default for DhtMessageTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use super::*;

    fn make_addr(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    fn make_node_id(byte: u8) -> NodeId {
        let mut id = [0u8; 20];
        id[0] = byte;
        NodeId(id)
    }

    // -- TrackerEntry tests --

    #[test]
    fn entry_not_timeout_initially() {
        let entry = TrackerEntry::new(
            NodeId::ZERO,
            make_addr(1234),
            vec![1, 2, 3, 4],
            "ping".into(),
            Duration::from_secs(10),
        );
        assert!(!entry.is_timeout());
    }

    #[test]
    fn entry_matches_same_tid_and_addr() {
        let entry = TrackerEntry::new(
            make_node_id(1),
            make_addr(5000),
            vec![0xAA, 0xBB],
            "find_node".into(),
            Duration::from_secs(10),
        );
        assert!(entry.matches(&[0xAA, 0xBB], &make_addr(5000)));
    }

    #[test]
    fn entry_no_match_different_tid() {
        let entry = TrackerEntry::new(
            make_node_id(1),
            make_addr(5000),
            vec![0xAA, 0xBB],
            "find_node".into(),
            Duration::from_secs(10),
        );
        assert!(!entry.matches(&[0xCC, 0xDD], &make_addr(5000)));
    }

    #[test]
    fn entry_no_match_different_addr() {
        let entry = TrackerEntry::new(
            make_node_id(1),
            make_addr(5000),
            vec![0xAA, 0xBB],
            "find_node".into(),
            Duration::from_secs(10),
        );
        assert!(!entry.matches(&[0xAA, 0xBB], &make_addr(6000)));
    }

    #[test]
    fn entry_extend_timeout_resets_dispatch() {
        let mut entry = TrackerEntry::new(
            NodeId::ZERO,
            make_addr(1234),
            vec![1],
            "ping".into(),
            Duration::from_millis(50),
        );
        assert!(!entry.is_timeout());
        std::thread::sleep(Duration::from_millis(60));
        assert!(entry.is_timeout());
        entry.extend_timeout();
        assert!(!entry.is_timeout());
    }

    #[test]
    fn entry_accessors() {
        let entry = TrackerEntry::new(
            make_node_id(42),
            make_addr(7000),
            vec![0x01, 0x02],
            "get_peers".into(),
            Duration::from_secs(5),
        );
        assert_eq!(entry.method(), "get_peers");
        assert_eq!(entry.target_node_id(), &make_node_id(42));
        assert_eq!(entry.target_addr(), make_addr(7000));
        assert_eq!(entry.transaction_id(), &[0x01, 0x02]);
    }

    // -- DhtMessageTracker tests --

    #[test]
    fn tracker_add_and_match() {
        let mut tracker = DhtMessageTracker::new();
        let addr = make_addr(5000);
        let tid = vec![1, 2, 3, 4];

        tracker.add_query(make_node_id(1), addr, tid.clone(), "ping".into());
        assert_eq!(tracker.count(), 1);

        let result = tracker.match_response(&tid, &addr).unwrap();
        assert_eq!(result.method, "ping");
        assert_eq!(result.target_node_id, make_node_id(1));
        assert_eq!(result.target_addr, addr);
        // Elapsed should be very small for an immediately-matched entry
        assert!(result.elapsed < Duration::from_secs(1));
        assert_eq!(tracker.count(), 0);
    }

    #[test]
    fn tracker_no_match_returns_none() {
        let mut tracker = DhtMessageTracker::new();
        tracker.add_query(make_node_id(1), make_addr(5000), vec![1, 2], "ping".into());

        assert!(tracker.match_response(&[1, 2], &make_addr(9999)).is_none());
        assert!(tracker.match_response(&[9, 9], &make_addr(5000)).is_none());
        let mut empty_tracker = DhtMessageTracker::new();
        assert!(
            empty_tracker
                .match_response(&[1, 2], &make_addr(5000))
                .is_none()
        );
    }

    #[test]
    fn tracker_multiple_entries_match_correct() {
        let mut tracker = DhtMessageTracker::new();
        let addr1 = make_addr(5001);
        let addr2 = make_addr(5002);

        tracker.add_query(make_node_id(1), addr1, vec![1], "ping".into());
        tracker.add_query(make_node_id(2), addr2, vec![2], "find_node".into());
        tracker.add_query(make_node_id(3), addr1, vec![3], "get_peers".into());

        assert_eq!(tracker.count(), 3);

        let result = tracker.match_response(&[2], &addr2).unwrap();
        assert_eq!(result.method, "find_node");
        assert_eq!(tracker.count(), 2);

        let result = tracker.match_response(&[1], &addr1).unwrap();
        assert_eq!(result.method, "ping");
        assert_eq!(tracker.count(), 1);

        let result = tracker.match_response(&[3], &addr1).unwrap();
        assert_eq!(result.method, "get_peers");
        assert!(tracker.is_empty());
    }

    #[test]
    fn tracker_timeout_removes_expired() {
        let mut tracker = DhtMessageTracker::with_timeout(Duration::from_millis(30));
        tracker.add_query(make_node_id(1), make_addr(5000), vec![1], "ping".into());
        tracker.add_query(
            make_node_id(2),
            make_addr(5001),
            vec![2],
            "find_node".into(),
        );

        assert_eq!(tracker.count(), 2);

        std::thread::sleep(Duration::from_millis(40));

        let timed_out = tracker.handle_timeout();
        assert_eq!(timed_out.len(), 2);
        assert_eq!(timed_out[0].method, "ping");
        assert_eq!(timed_out[1].method, "find_node");
        assert!(tracker.is_empty());
    }

    #[test]
    fn tracker_timeout_keeps_non_expired() {
        let mut tracker = DhtMessageTracker::new();
        tracker.add_query_with_timeout(
            make_node_id(1),
            make_addr(5000),
            vec![1],
            "ping".into(),
            Duration::from_millis(20),
        );
        tracker.add_query_with_timeout(
            make_node_id(2),
            make_addr(5001),
            vec![2],
            "find_node".into(),
            Duration::from_secs(300),
        );

        std::thread::sleep(Duration::from_millis(30));

        let timed_out = tracker.handle_timeout();
        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0].method, "ping");
        assert_eq!(tracker.count(), 1);

        let entry = tracker.get_entry(&[2]).unwrap();
        assert_eq!(entry.method(), "find_node");
    }

    #[test]
    fn tracker_timeout_entry_has_elapsed() {
        let mut tracker = DhtMessageTracker::with_timeout(Duration::from_millis(10));
        tracker.add_query(make_node_id(1), make_addr(5000), vec![1], "ping".into());

        std::thread::sleep(Duration::from_millis(20));

        let timed_out = tracker.handle_timeout();
        assert_eq!(timed_out.len(), 1);
        assert!(timed_out[0].elapsed >= Duration::from_millis(10));
    }

    #[test]
    fn tracker_get_entry_by_tid() {
        let mut tracker = DhtMessageTracker::new();
        tracker.add_query(make_node_id(1), make_addr(5000), vec![0xAA], "ping".into());
        tracker.add_query(
            make_node_id(2),
            make_addr(5001),
            vec![0xBB],
            "find_node".into(),
        );

        let entry = tracker.get_entry(&[0xBB]).unwrap();
        assert_eq!(entry.method(), "find_node");
        assert_eq!(*entry.target_node_id(), make_node_id(2));

        assert!(tracker.get_entry(&[0xCC]).is_none());
    }

    #[test]
    fn tracker_match_removes_entry_so_no_double_match() {
        let mut tracker = DhtMessageTracker::new();
        let addr = make_addr(5000);
        tracker.add_query(make_node_id(1), addr, vec![1], "ping".into());

        assert!(tracker.match_response(&[1], &addr).is_some());
        assert!(tracker.match_response(&[1], &addr).is_none());
    }

    #[test]
    fn tracker_default_impl() {
        let tracker = DhtMessageTracker::default();
        assert!(tracker.is_empty());
        assert_eq!(tracker.count(), 0);
    }
}
