use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;


use crate::engine::bt_peer_blocklist::BtPeerBlocklist;
use crate::engine::bt_peer_storage::constants::{CHOKE_ROUND_INTERVAL_SECS, MAX_DROPPED_PEERS};
use crate::engine::bt_peer_storage::peer_entry::PeerEntry;
use crate::engine::peer_stats::PeerStats;
use super::DefaultPeerStorage;

impl DefaultPeerStorage {
    // ==================================================================
    // Choking integration
    // ==================================================================

    /// Check whether a choke round interval (10s) has elapsed.
    ///
    /// Delegates to the appropriate choke algorithm (seeder or leecher)
    /// based on whether the download is finished.
    ///
    /// Matches C++ DefaultPeerStorage::chokeRoundIntervalElapsed.
    pub fn choke_round_interval_elapsed(&self) -> bool {
        let choke_interval = Duration::from_secs(CHOKE_ROUND_INTERVAL_SECS);

        if self.download_finished {
            self.seeder_state_choke.should_execute(choke_interval)
        } else {
            self.leecher_state_choke.should_execute(choke_interval)
        }
    }

    /// Execute a choke round on the given peers.
    ///
    /// If the download is finished, delegates to the seeder choke algorithm.
    /// Otherwise, delegates to the leecher choke algorithm.
    ///
    /// Matches C++ DefaultPeerStorage::executeChoke.
    pub fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) {
        if self.download_finished {
            self.seeder_state_choke.execute_choke(peers);
        } else {
            self.leecher_state_choke.execute_choke(peers);
        }
    }

    // ==================================================================
    // Accessors
    // ==================================================================

    /// Total count of tracked peers (unused + used).
    ///
    /// Matches C++ DefaultPeerStorage::countAllPeer.
    pub fn count_all_peers(&self) -> usize {
        self.unused_peers.len() + self.used_peers.len()
    }

    /// Get a reference to the unused peers list.
    pub fn unused_peers(&self) -> &VecDeque<PeerEntry> {
        &self.unused_peers
    }

    /// Get a reference to the used peers set.
    pub fn used_peers(&self) -> &HashSet<PeerEntry> {
        &self.used_peers
    }

    /// Get a reference to the dropped peers list.
    pub fn dropped_peers(&self) -> &VecDeque<PeerEntry> {
        &self.dropped_peers
    }

    // ==================================================================
    // Configuration setters
    // ==================================================================

    /// Set the maximum peer list size.
    pub fn set_max_peer_list_size(&mut self, size: usize) {
        self.max_peer_list_size = size;
    }

    /// Set whether the download has finished (affects choke algorithm).
    pub fn set_download_finished(&mut self, finished: bool) {
        self.download_finished = finished;
    }

    /// Set whether piece storage is available.
    pub fn set_piece_storage_available(&mut self, available: bool) {
        self.piece_storage_available = available;
    }

    /// Set the peer blocklist for IP-based rejection.
    ///
    /// In C++ aria2, the blocklist is passed to the constructor. Here we
    /// use a setter to allow flexible construction order.
    pub fn set_peer_blocklist(&mut self, blocklist: Arc<BtPeerBlocklist>) {
        self.peer_blocklist = Some(blocklist);
    }

    /// Get the number of peers rejected by the blocklist.
    pub fn blocklist_reject_count(&self) -> u64 {
        self.blocklist_reject_count
    }

    // ==================================================================
    // Internal helpers
    // ==================================================================

    /// Check whether a peer IP is blocked by the blocklist.
    ///
    /// Returns false if no blocklist is configured.
    pub(super) fn is_blocked_by_blocklist(&self, ipaddr: &str) -> bool {
        match &self.peer_blocklist {
            Some(bl) => bl.contains(ipaddr),
            None => false,
        }
    }

    /// Add a peer to the dropped list, evicting duplicates and capping at
    /// MAX_DROPPED_PEERS.
    ///
    /// Matches C++ DefaultPeerStorage::addDroppedPeer.
    pub(super) fn add_dropped_peer(&mut self, peer: &PeerEntry) {
        // Remove any existing entry with the same (ip, port) to avoid
        // duplicates -- the new entry replaces the old one.
        if let Some(pos) = self
            .dropped_peers
            .iter()
            .position(|p| p.ip == peer.ip && p.port == peer.port)
        {
            self.dropped_peers.remove(pos);
        }

        self.dropped_peers.push_front(peer.clone());

        // Cap at MAX_DROPPED_PEERS (C++ hardcodes 50).
        while self.dropped_peers.len() > MAX_DROPPED_PEERS {
            self.dropped_peers.pop_back();
        }
    }

    /// Verify internal invariant: uniq_peers == keys(unused) U keys(used).
    ///
    /// This mirrors the C++ destructor assertion:
    /// assert(uniqPeers_.size() == unusedPeers_.size() + usedPeers_.size()).
    #[cfg(test)]
    pub(in crate::engine::bt_peer_storage) fn verify_invariant(&self) {
        assert_eq!(
            self.uniq_peers.len(),
            self.unused_peers.len() + self.used_peers.len(),
            "Invariant violated: uniq_peers.len() != unused_peers.len() + used_peers.len()"
        );
    }
}
