//! PeerEntry — lightweight peer descriptor for peer storage tracking.

use crate::engine::peer_stats::PeerStats;
use std::sync::Arc;

/// Lightweight peer descriptor for peer storage tracking.
///
/// Unlike the full `BtPeerConn`, this struct only tracks the fields needed
/// for peer lifecycle management (add/checkout/return/drop) and choking
/// algorithm decisions.
///
/// # Identity
///
/// Two `PeerEntry` values are considered equal if they share the same
/// `(ip, port)` pair. All other fields are ignored for `Hash`/`Eq`/`PartialEq`.
/// This matches the C++ `uniqPeers_` deduplication behavior.
#[derive(Clone, Debug)]
pub struct PeerEntry {
    /// IP address (or hostname) of the peer.
    pub ip: Arc<str>,
    /// Port number of the peer.
    pub port: u16,
    /// Caretaker unique ID that "owns" this peer (0 = not checked out).
    pub used_by: u64,
    /// Whether the connection is currently active.
    pub is_active: bool,
    /// Whether we are choking this peer.
    pub am_choking: bool,
    /// Whether the peer is interested in our data.
    pub peer_interested: bool,
    /// Whether this is an incoming (rather than outgoing) connection.
    pub is_incoming: bool,
    /// Whether the peer disconnected gracefully (sent proper close).
    pub disconnected_gracefully: bool,
}

impl PeerEntry {
    /// Create a new `PeerEntry` with default state (not checked out, not active).
    pub fn new(ip: String, port: u16) -> Self {
        Self {
            ip: Arc::from(ip),
            port,
            used_by: 0,
            is_active: false,
            am_choking: true,
            peer_interested: false,
            is_incoming: false,
            disconnected_gracefully: false,
        }
    }

    /// Create a `PeerEntry` from a `PeerStats` reference.
    ///
    /// This is a convenience conversion for feeding choking algorithm
    /// output back into peer storage.
    pub fn from_peer_stats(ip: String, port: u16, stats: &PeerStats) -> Self {
        Self {
            ip: Arc::from(ip),
            port,
            used_by: 0,
            is_active: !stats.is_banned,
            am_choking: stats.am_choking,
            peer_interested: stats.peer_interested,
            is_incoming: false,
            disconnected_gracefully: false,
        }
    }
}

// Identity is based on (ip, port) only — matching C++ uniqPeers_ behavior.
impl PartialEq for PeerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.ip == other.ip && self.port == other.port
    }
}

impl Eq for PeerEntry {}

impl std::hash::Hash for PeerEntry {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ip.hash(state);
        self.port.hash(state);
    }
}
