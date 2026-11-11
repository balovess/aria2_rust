//! BitTorrent registry blocklist operations.

use super::super::BtRegistry;

impl BtRegistry {
    // -----------------------------------------------------------------------
    // Peer blocklist
    // -----------------------------------------------------------------------

    /// Get a reference to the peer blocklist.
    ///
    /// Equivalent to C++ `BtRegistry::getPeerBlocklist()`.
    pub fn peer_blocklist(&self) -> &crate::engine::bittorrent::peer::blocklist::BtPeerBlocklist {
        &self.peer_blocklist
    }

    /// Get a mutable reference to the peer blocklist.
    ///
    /// Use this to add or remove blocklist rules.
    pub fn peer_blocklist_mut(
        &mut self,
    ) -> &mut crate::engine::bittorrent::peer::blocklist::BtPeerBlocklist {
        &mut self.peer_blocklist
    }

    /// Convenience method: check if a peer IP address is in the blocklist.
    ///
    /// Returns `true` if the address matches any blocked range.
    pub fn is_peer_blocked(&self, addr: &str) -> bool {
        self.peer_blocklist.contains(addr)
    }

    // -----------------------------------------------------------------------
}
