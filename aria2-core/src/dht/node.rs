//! DHT node representation.
//!
//! A `DhtNode` represents a participant in the DHT network, identified by
//! a 20-byte node ID. This module tracks node state including network
//! address, RTT, condition (failure count), and last contact time.
//!
//! # Node Condition States
//!
//! - **Good**: Recently contacted and no failures. `condition < BAD_CONDITION`
//!   and `last_contact` within `NODE_CONTACT_INTERVAL`.
//! - **Questionable**: Not recently contacted but not bad. `condition < BAD_CONDITION`
//!   but `last_contact` is stale.
//! - **Bad**: Too many timeouts. `condition >= BAD_CONDITION`.
//!
//! C++ reference: `DHTNode.h/cc`

use std::cmp::Ordering;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tracing::trace;

use super::constants::{BAD_CONDITION, NODE_CONTACT_INTERVAL_SECS};
use super::node_id::NodeId;

/// A DHT node in the routing table.
///
/// Tracks the node's identity, network address, and health state.
/// The condition counter increments on each timeout and resets on
/// successful contact, matching the C++ behavior exactly.
#[derive(Clone, Debug)]
pub struct DhtNode {
    /// 20-byte node identifier.
    id: NodeId,
    /// Network address (IP + port).
    addr: SocketAddr,
    /// Round-trip time in milliseconds.
    rtt_ms: u64,
    /// Failure counter. When >= BAD_CONDITION, node is "bad".
    condition: u32,
    /// Instant of last successful contact.
    last_contact: Option<Instant>,
}

impl DhtNode {
    /// Create a new node with the given ID and address.
    ///
    /// The condition starts at 1 (meaning "known but unverified"), matching
    /// the C++ `DHTNode(const unsigned char* id)` constructor which sets
    /// `condition_ = 1`.
    pub fn new(id: NodeId, addr: SocketAddr) -> Self {
        DhtNode {
            id,
            addr,
            rtt_ms: 0,
            condition: 1,
            last_contact: None,
        }
    }

    /// Create a node with a randomly generated ID.
    ///
    /// C++: `DHTNode()` default constructor calls `generateID()`.
    pub fn with_random_id(addr: SocketAddr) -> Self {
        DhtNode {
            id: NodeId::random(),
            addr,
            rtt_ms: 0,
            condition: 0,
            last_contact: None,
        }
    }

    /// Return the node ID.
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// Return the network address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Set the network address.
    pub fn set_addr(&mut self, addr: SocketAddr) {
        self.addr = addr;
    }

    /// Return the RTT in milliseconds.
    pub fn rtt_ms(&self) -> u64 {
        self.rtt_ms
    }

    /// Update the RTT measurement.
    ///
    /// C++: `updateRTT(std::chrono::milliseconds t)`
    pub fn update_rtt(&mut self, rtt_ms: u64) {
        self.rtt_ms = rtt_ms;
    }

    /// Return the condition (failure count).
    pub fn condition(&self) -> u32 {
        self.condition
    }

    /// Check if the node is "good" (recently contacted and not failing).
    ///
    /// C++: `isGood()` returns `!isBad() && !isQuestionable()`
    pub fn is_good(&self) -> bool {
        !self.is_bad() && !self.is_questionable()
    }

    /// Check if the node is "bad" (too many timeouts).
    ///
    /// C++: `isBad()` returns `condition_ >= BAD_CONDITION`
    pub fn is_bad(&self) -> bool {
        self.condition >= BAD_CONDITION
    }

    /// Check if the node is "questionable" (stale but not bad).
    ///
    /// A node is questionable if it hasn't been contacted recently.
    /// C++: `isQuestionable()` returns `!isBad() && lastContact_.difference(wallclock) >= DHT_NODE_CONTACT_INTERVAL`
    pub fn is_questionable(&self) -> bool {
        if self.is_bad() {
            return false;
        }
        match self.last_contact {
            Some(t) => t.elapsed() >= Duration::from_secs(NODE_CONTACT_INTERVAL_SECS),
            None => true,
        }
    }

    /// Mark the node as good (reset condition counter).
    ///
    /// C++: `markGood()` sets `condition_ = 0`
    pub fn mark_good(&mut self) {
        self.condition = 0;
    }

    /// Mark the node as bad (set condition to BAD_CONDITION).
    ///
    /// C++: `markBad()` sets `condition_ = BAD_CONDITION`
    pub fn mark_bad(&mut self) {
        self.condition = BAD_CONDITION;
    }

    /// Update the last contact time to now.
    ///
    /// C++: `updateLastContact()` sets `lastContact_ = wallclock`
    pub fn update_last_contact(&mut self) {
        self.last_contact = Some(Instant::now());
    }

    /// Mark this node as successfully contacted (good + last contact updated).
    ///
    /// Equivalent to calling `mark_good()` + `update_last_contact()`.
    /// C++: done in `DHTPingReplyMessage::receivedAction()` and
    /// `DHTAbstractNodeLookupTask::onReceived()`.
    pub fn mark_contacted(&mut self) {
        self.mark_good();
        self.update_last_contact();
    }

    /// Record a timeout (increment condition counter).
    ///
    /// C++: `timeout()` does `++condition_`
    pub fn timeout(&mut self) {
        self.condition += 1;
        trace!(node_id = %self.id, condition = self.condition, "DHT node timeout");
    }

    /// Return the last contact instant, if any.
    pub fn last_contact(&self) -> Option<Instant> {
        self.last_contact
    }

    /// Set the last contact time from an external instant (for deserialization).
    pub fn set_last_contact(&mut self, instant: Option<Instant>) {
        self.last_contact = instant;
    }
}

/// Nodes are compared by their ID only, matching the C++ operator==.
impl PartialEq for DhtNode {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for DhtNode {}

/// Ordering by node ID (lexicographic), matching C++ `operator<`.
impl Ord for DhtNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for DhtNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for DhtNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "DhtNode ID={}, Addr={}, Condition={}, RTT={}ms",
            self.id, self.addr, self.condition, self.rtt_ms
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 6881)
    }

    #[test]
    fn new_node_has_condition_1() {
        let node = DhtNode::new(NodeId::ZERO, test_addr());
        assert_eq!(node.condition(), 1);
        assert!(!node.is_good()); // condition=1 but no last_contact -> questionable
    }

    #[test]
    fn random_id_node_has_condition_0() {
        let node = DhtNode::with_random_id(test_addr());
        assert_eq!(node.condition(), 0);
    }

    #[test]
    fn mark_good_resets_condition() {
        let mut node = DhtNode::new(NodeId::ZERO, test_addr());
        node.timeout();
        node.timeout();
        assert_eq!(node.condition(), 3);
        node.mark_good();
        assert_eq!(node.condition(), 0);
    }

    #[test]
    fn mark_bad_sets_condition() {
        let mut node = DhtNode::new(NodeId::ZERO, test_addr());
        node.mark_bad();
        assert!(node.is_bad());
        assert_eq!(node.condition(), BAD_CONDITION);
    }

    #[test]
    fn timeout_increments_condition() {
        let mut node = DhtNode::new(NodeId::ZERO, test_addr());
        for _ in 0..BAD_CONDITION {
            node.timeout();
        }
        assert!(node.is_bad());
    }

    #[test]
    fn update_last_contact_makes_node_good() {
        let mut node = DhtNode::new(NodeId::ZERO, test_addr());
        node.mark_good();
        node.update_last_contact();
        assert!(node.is_good());
    }

    #[test]
    fn equality_by_id() {
        let a = DhtNode::new(NodeId::ZERO, test_addr());
        let b = DhtNode::new(
            NodeId::ZERO,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 9999),
        );
        assert_eq!(a, b); // same ID, different address -> still equal
    }

    #[test]
    fn inequality_by_id() {
        let a = DhtNode::new(NodeId::ZERO, test_addr());
        let b = DhtNode::new(NodeId::MAX, test_addr());
        assert_ne!(a, b);
    }

    #[test]
    fn rtt_update() {
        let mut node = DhtNode::new(NodeId::ZERO, test_addr());
        node.update_rtt(42);
        assert_eq!(node.rtt_ms(), 42);
    }

    #[test]
    fn questionable_when_no_contact() {
        let mut node = DhtNode::new(NodeId::ZERO, test_addr());
        node.mark_good();
        // No last_contact -> questionable
        assert!(node.is_questionable());
    }

    #[test]
    fn not_questionable_when_recently_contacted() {
        let mut node = DhtNode::new(NodeId::ZERO, test_addr());
        node.mark_good();
        node.update_last_contact();
        assert!(!node.is_questionable());
        assert!(node.is_good());
    }

    #[test]
    fn display_format() {
        let node = DhtNode::new(NodeId::ZERO, test_addr());
        let s = format!("{}", node);
        assert!(s.contains("DhtNode"));
        assert!(s.contains("0000")); // hex of zero ID
    }
}
