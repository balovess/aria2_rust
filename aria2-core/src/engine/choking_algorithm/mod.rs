//! BitTorrent choking algorithm implementation (tit-for-tat strategy)
//!
//! Module structure:
//! - [ChokingAlgorithm] - Main struct and public API
//! - [selection] - Unchoke candidate selection (tit-for-tat rotation)
//! - [optimistic] - Optimistic unchoke logic (round-robin)
//! - [	ests] - Comprehensive test suite

mod selection;
mod optimistic;

use std::collections::HashSet;

use super::peer_stats::PeerStats;

/// Action to take for a peer during choke rotation
#[derive(Debug, Clone, PartialEq)]
pub enum ChokeAction {
    /// Unchoke peer at index
    Unchoke(usize),
    /// Choke peer at index
    Choke(usize),
    /// No action needed for this peer
    NoChange(usize),
}

/// Configuration for the choking algorithm
#[derive(Debug, Clone)]
pub struct ChokingConfig {
    /// Maximum number of peers to unchoke simultaneously (default: 4)
    pub max_upload_slots: usize,
    /// Interval in seconds between optimistic unchokes (default: 30)
    pub optimistic_unchoke_interval_secs: u64,
    /// Timeout in seconds after which a peer is considered snubbed (default: 60)
    pub snubbed_timeout_secs: u64,
    /// Interval in seconds between choke rotations (default: 10)
    pub choke_rotation_interval_secs: u64,
}

impl Default for ChokingConfig {
    fn default() -> Self {
        Self {
            max_upload_slots: 4,
            optimistic_unchoke_interval_secs: 30,
            snubbed_timeout_secs: 60,
            choke_rotation_interval_secs: 10,
        }
    }
}

/// BitTorrent choking algorithm implementation (tit-for-tat strategy)
///
/// This implements the standard BT choking algorithm:
/// - Top K peers by score get unchoked (reciprocity-based)
/// - One additional slot for optimistic unchoke (random selection)
/// - Snubbed peers are penalized heavily
///
/// The algorithm minimizes churn by only changing state when necessary.
pub struct ChokingAlgorithm {
    pub(crate) peers: Vec<PeerStats>,
    pub(crate) config: ChokingConfig,
    /// Explicitly snubbed peer indices (separate from PeerStats.is_snubbed for
    /// algorithm-level control). Peers in this set always receive score -1000.
    pub(crate) snubbed_peers: HashSet<usize>,
    /// Index of the current optimistically unchoked peer (for rotation).
    pub(crate) current_optimistic_peer: Option<usize>,
    /// Round-robin counter for optimistic unchoke rotation.
    pub(crate) optimistic_rotation_counter: usize,
}

impl ChokingAlgorithm {
    /// Create a new choking algorithm with the given configuration
    pub fn new(config: ChokingConfig) -> Self {
        Self {
            peers: Vec::new(),
            config,
            snubbed_peers: HashSet::new(),
            current_optimistic_peer: None,
            optimistic_rotation_counter: 0,
        }
    }

    /// Add a peer to be managed by the algorithm
    pub fn add_peer(&mut self, stats: PeerStats) {
        self.peers.push(stats);
    }

    /// Remove a peer at the given index
    pub fn remove_peer(&mut self, idx: usize) {
        if idx < self.peers.len() {
            self.peers.remove(idx);
        }
    }

    /// Returns the number of peers being managed
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Returns true if there are no peers
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Core algorithm: called every ~10 seconds (config.choke_rotation_interval_secs)
    ///
    /// This performs the tit-for-tat choke rotation:
    /// 1. Check and mark snubbed peers (timeout-based)
    /// 2. Calculate score for each peer
    /// 3. Sort by score descending
    /// 4. Top K get Unchoke, rest get Choke
    ///    BUT: keep currently unchoked peers unchoked if they're still in top K
    ///    (avoid churn - only change what's necessary)
    /// 5. Return only the actions that changed state
    pub fn rotate_choke(&mut self) -> Vec<ChokeAction> {
        selection::rotate_choke(self)
    }

    /// Called every ~30 seconds (config.optimistic_unchoke_interval_secs)
    ///
    /// Selects ONE choked+interested peer for optimistic unchoke.
    /// This gives new/unknown peers a chance to prove themselves.
    ///
    /// Uses round-robin rotation among eligible non-snubbed peers
    /// to ensure fair distribution of the optimistic unchoke slot.
    ///
    /// Returns Some(index) if found, None if no eligible peer
    pub fn optimistically_unchoke(&mut self) -> Option<usize> {
        optimistic::optimistically_unchoke(self)
    }

    /// Rotate which peer gets the optimistic unchoke slot using round-robin.
    ///
    /// Picks a different peer than the current one when possible,
    /// cycling through eligible peers in order.
    ///
    /// # Arguments
    /// * eligible_peers - Indices of peers that are eligible for optimistic unchoke
    ///
    /// # Returns
    /// The index of the selected peer from the eligible set
    pub fn rotate_optimistic_unchoked(&mut self, eligible_peers: &[usize]) -> usize {
        optimistic::rotate_optimistic_unchoked(self, eligible_peers)
    }

    /// Called whenever we receive data from a peer.
    /// Automatically unsnubs the peer if it was in the explicit snubbed set.
    pub fn on_data_received(&mut self, peer_idx: usize, bytes: u64) {
        if let Some(peer) = self.peers.get_mut(peer_idx) {
            peer.on_data_received(bytes);
        }
        // Auto-unsnub: receiving data means the peer is responsive again
        self.unsnub_peer(peer_idx);
    }

    /// Explicitly mark a peer as snubbed (algorithm-level).
    ///
    /// This adds the peer to the snubbed_peers set, which causes
    /// calculate_peer_score to return -1000 for this peer, ensuring
    /// they always get choked on the next rotation.
    pub fn mark_peer_snubbed(&mut self, peer_id: usize) {
        if self.snubbed_peers.insert(peer_id) {
            tracing::debug!("[BT] Peer {} explicitly marked as snubbed", peer_id);
        }
    }

    /// Remove a peer from the explicit snubbed set (they sent data again).
    ///
    /// Returns 	rue if the peer was actually in the snubbed set (newly un-snubbed),
    /// alse if they were not snubbed.
    pub fn unsnub_peer(&mut self, peer_id: usize) -> bool {
        if self.snubbed_peers.remove(&peer_id) {
            tracing::debug!("[BT] Peer {} un-snubbed (data received)", peer_id);
            true
        } else {
            false
        }
    }

    /// Check if a peer is in the explicit snubbed set.
    pub fn is_explicitly_snubbed(&self, peer_id: usize) -> bool {
        self.snubbed_peers.contains(&peer_id)
    }

    /// Get the number of explicitly snubbed peers.
    pub fn snubbed_count(&self) -> usize {
        self.snubbed_peers.len()
    }

    /// Check all peers for snubbed status
    /// Returns indices of newly snubbed peers
    pub fn check_snubbed_peers(&mut self) -> Vec<usize> {
        selection::check_snubbed_peers_internal(self)
    }

    /// Score function: higher = better peer to keep unchoked
    ///
    /// Score components:
    ///   1. Download speed contribution (how much they give us): weight 0.5
    ///   2. Upload speed contribution (reciprocity): weight 0.3
    ///   3. Snubbed penalty: -1000 if snubbed (either in PeerStats or algorithm set)
    ///   4. Interest bonus: +50 if peer_interested
    ///   5. New peer bonus (time since unchoke < 60s): +30 (anti-churn)
    #[allow(dead_code)] // Used by tests via ChokingAlgorithm::calculate_peer_score
    pub(crate) fn calculate_peer_score(peer: &PeerStats, is_explicitly_snubbed: bool) -> f64 {
        selection::calculate_peer_score(peer, is_explicitly_snubbed)
    }

    /// Get mutable reference to peer stats
    pub fn get_peer_mut(&mut self, idx: usize) -> Option<&mut PeerStats> {
        self.peers.get_mut(idx)
    }

    /// Get reference to peer stats
    pub fn get_peer(&self, idx: usize) -> Option<&PeerStats> {
        self.peers.get(idx)
    }

    /// Get all peers as a slice
    pub fn peers(&self) -> &[PeerStats] {
        &self.peers
    }

    /// Get reference to configuration
    pub fn config(&self) -> &ChokingConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests;
